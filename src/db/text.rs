//! Text encodings of values, for connectors to share: PostgreSQL's `COPY` text format
//! (also spoken by PostgreSQL-compatible servers), and SQL literals and multi-row
//! `INSERT ... VALUES` statements in a [`TextStyle`] the connector chooses.

use std::fmt::Write as _;

use crate::schema::DataType;
use crate::timeutil::{SqlTimestamp, Ymd};
use crate::value::{Value, format_decimal, format_uuid};

/// How a database spells booleans and escapes strings in SQL text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    /// The spelling of true: `1`, `t`, `TRUE`.
    pub true_text: &'static str,
    /// The spelling of false: `0`, `f`, `FALSE`.
    pub false_text: &'static str,
    /// String literals escape `'` and `\` with a backslash and write NUL as `\0` (MySQL
    /// unless `NO_BACKSLASH_ESCAPES`), rather than doubling `'` (standard SQL).
    pub backslash_escapes: bool,
}

/// The style of PostgreSQL's `COPY` text format: booleans as `t`/`f`.
const COPY_STYLE: TextStyle = TextStyle { true_text: "t", false_text: "f", backslash_escapes: false };

/// The canonical text of a non-NULL scalar value for a column of type `ty`: numbers in
/// full, timestamps as `YYYY-MM-DD HH:MM:SS[.f]` in UTC with the column's fractional
/// digits, dates as `YYYY-MM-DD`, UUIDs and IP addresses in their usual form.
pub fn scalar_text(ty: &DataType, v: &Value, style: TextStyle) -> Result<String, String> {
    Ok(match (ty.base(), v) {
        (_, Value::Bool(b)) => if *b { style.true_text } else { style.false_text }.to_string(),
        (DataType::Bool, other) => match other.truth() {
            Some(b) => return scalar_text(ty, &Value::Bool(b), style),
            None => return Err(format!("{} for a boolean column", other.type_name())),
        },
        (_, Value::Int(i)) => i.to_string(),
        (_, Value::Float(f)) if f.is_finite() => f.to_string(),
        (_, Value::Float(f)) => return Err(format!("{f} is not a finite number")),
        (_, Value::Decimal(d, s)) => format_decimal(*d, *s),
        (_, Value::Str(s)) => s.clone(),
        (_, Value::Uuid(u)) => format_uuid(*u),
        (_, Value::Date(d)) => Ymd(i64::from(*d)).to_string(),
        (DataType::Date | DataType::Date32, Value::Time(t)) => Ymd(crate::timeutil::day_of(*t)).to_string(),
        (DataType::DateTime { precision, .. }, Value::Time(t)) => timestamp(*t, precision.unwrap_or(0)),
        (_, Value::Time(t)) => timestamp(*t, 6),
        (_, Value::Ip4(ip)) => std::net::Ipv4Addr::from(*ip).to_string(),
        (_, Value::Ip6(ip)) => std::net::Ipv6Addr::from(*ip).to_string(),
        (_, other) => return Err(format!("{} values are not supported in this database", other.type_name())),
    })
}

/// `YYYY-MM-DD HH:MM:SS[.f]` with `digits` fractional digits (at most 6).
fn timestamp(us: i64, digits: u8) -> String {
    let full = SqlTimestamp(us).to_string();
    match digits.min(6) {
        0 => full[..19].to_string(),
        d => full[..20 + d as usize].to_string(),
    }
}

/// Appends one field in PostgreSQL's `COPY` text format.
pub fn copy_field(out: &mut Vec<u8>, ty: &DataType, v: &Value) -> Result<(), String> {
    if v.is_null() {
        out.extend_from_slice(b"\\N");
        return Ok(());
    }
    let text = match (ty.base(), v) {
        (DataType::Array(elem), Value::Array(items)) => postgres_array(elem, items)?,
        (_, Value::Array(_) | Value::Map(_) | Value::Tuple(_)) => return Err(format!("{} values are not supported by PostgreSQL", v.type_name())),
        _ => scalar_text(ty, v, COPY_STYLE)?,
    };
    for b in text.bytes() {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(b),
        }
    }
    Ok(())
}

/// A PostgreSQL array literal: `{"a","b",NULL}`. Every element is quoted, which is valid
/// for any element type.
fn postgres_array(elem: &DataType, items: &[Value]) -> Result<String, String> {
    let mut s = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match item {
            Value::Null => s.push_str("NULL"),
            Value::Array(inner) => s.push_str(&postgres_array(elem.base(), inner)?),
            other => {
                let text = scalar_text(elem, other, COPY_STYLE)?;
                let _ = write!(s, "\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
            }
        }
    }
    s.push('}');
    Ok(s)
}

/// Appends a SQL literal for an `INSERT ... VALUES` statement: `NULL`, numbers and
/// booleans bare, everything else quoted. Arrays, maps and tuples are refused.
pub fn sql_literal(out: &mut Vec<u8>, ty: &DataType, v: &Value, style: TextStyle) -> Result<(), String> {
    match v {
        Value::Null => out.extend_from_slice(b"NULL"),
        Value::Array(_) | Value::Map(_) | Value::Tuple(_) => return Err(format!("{} values are not supported in this database", v.type_name())),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Decimal(..) => out.extend_from_slice(scalar_text(ty, v, style)?.as_bytes()),
        _ if matches!(ty.base(), DataType::Bool) => out.extend_from_slice(scalar_text(ty, v, style)?.as_bytes()),
        _ => quoted(out, &scalar_text(ty, v, style)?, style),
    }
    Ok(())
}

fn quoted(out: &mut Vec<u8>, s: &str, style: TextStyle) {
    out.push(b'\'');
    for b in s.bytes() {
        match (b, style.backslash_escapes) {
            (b'\'', true) => out.extend_from_slice(b"\\'"),
            (b'\\', true) => out.extend_from_slice(b"\\\\"),
            (0, true) => out.extend_from_slice(b"\\0"),
            (b'\'', _) => out.extend_from_slice(b"''"),
            _ => out.push(b),
        }
    }
    out.push(b'\'');
}

/// Appends a row as `(lit, lit, ...)`, for [`insert_statements`].
pub fn sql_row(out: &mut Vec<u8>, row: &super::RowRef<'_>, style: TextStyle) -> Result<(), super::CellError> {
    out.push(b'(');
    for (k, ty, v) in row.cells() {
        if k > 0 {
            out.push(b',');
        }
        sql_literal(out, ty, v, style).map_err(|message| super::CellError { column: k, message })?;
    }
    out.push(b')');
    Ok(())
}

/// Builds `INSERT` statements for a batch, each holding at most about `max_bytes` of
/// rows, so no statement exceeds the server's limit on statement size.
pub fn insert_statements(prefix: &str, batch: &super::Batch, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for row in batch.row_slices() {
        if current.is_empty() {
            current.push_str(prefix);
        } else {
            current.push(',');
        }
        current.push_str(&String::from_utf8_lossy(row));
        if current.len() >= max_bytes {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_fields_escape_and_format() {
        let mut out = Vec::new();
        copy_field(&mut out, &DataType::String { max_len: None }, &Value::Str("a\tb\\c\n".into())).unwrap();
        assert_eq!(out, b"a\\tb\\\\c\\n");
        out.clear();
        let arr = DataType::Array(Box::new(DataType::String { max_len: None }));
        copy_field(&mut out, &arr, &Value::Array(vec![Value::Str("x \"y\"".into()), Value::Null])).unwrap();
        // Quotes inside the element are escaped for the array, then the backslashes for COPY.
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"x \\"y\\"",NULL}"#);
        let ts = DataType::DateTime { precision: Some(3), tz: None };
        assert_eq!(scalar_text(&ts, &Value::Time(1_704_110_400_123_456), COPY_STYLE).unwrap(), "2024-01-01 12:00:00.123");
        assert_eq!(scalar_text(&DataType::Bool, &Value::Bool(true), COPY_STYLE).unwrap(), "t");
    }

    #[test]
    fn sql_literals_follow_the_style() {
        let backslashes = TextStyle { true_text: "1", false_text: "0", backslash_escapes: true };
        let standard = TextStyle { true_text: "1", false_text: "0", backslash_escapes: false };
        let text = DataType::String { max_len: None };
        let lit = |v: &Value, style: TextStyle| {
            let mut out = Vec::new();
            sql_literal(&mut out, &text, v, style).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(lit(&Value::Str("it's \\".into()), backslashes), r"'it\'s \\'");
        assert_eq!(lit(&Value::Str("it's \\".into()), standard), r"'it''s \'");
        assert_eq!(lit(&Value::Null, standard), "NULL");
        assert_eq!(lit(&Value::Date(19_782), backslashes), "'2024-02-29'");
        assert_eq!(lit(&Value::Bool(true), backslashes), "1");
    }

    #[test]
    fn statements_split_at_row_boundaries() {
        let mut batch = super::super::Batch::default();
        for r in ["(1)", "(2)", "(3)"] {
            batch.body.extend_from_slice(r.as_bytes());
            batch.row_ends.push(batch.body.len());
        }
        assert_eq!(insert_statements("INSERT INTO t VALUES ", &batch, 1 << 20), vec!["INSERT INTO t VALUES (1),(2),(3)"]);
        assert_eq!(insert_statements("V ", &batch, 5).len(), 3);
    }
}
