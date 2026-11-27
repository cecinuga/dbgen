//! Column data types, dialect-neutral.
//!
//! A connector maps its type syntax onto these (`Connector::parse_type`, `TypeRules`),
//! and its backend maps them onto its wire format. The variants follow the richest type
//! system supported so far (ClickHouse); simpler systems map onto a subset.

use std::fmt;

use serde::Serialize;

/// A column type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Bool,
    Int {
        bits: u16,
        signed: bool,
    },
    Float {
        bits: u16,
    },
    Decimal {
        precision: u8,
        scale: u8,
    },
    /// Variable-length text; `max_len` in characters when the dialect bounds it.
    String {
        max_len: Option<u32>,
    },
    FixedString(u32),
    Uuid,
    /// 16-bit day count (ClickHouse `Date`, 1970-2149).
    Date,
    /// 32-bit day count (ClickHouse `Date32`, SQL `DATE`).
    Date32,
    /// `precision` None: 32-bit seconds (ClickHouse `DateTime`); Some(p): 64-bit ticks of
    /// 10^-p seconds (`DateTime64(p)`, SQL `TIMESTAMP(p)`).
    DateTime {
        precision: Option<u8>,
        tz: Option<String>,
    },
    Enum {
        bits: u8,
        variants: Vec<(String, i16)>,
    },
    Ipv4,
    Ipv6,
    Json,
    Array(Box<DataType>),
    Map(Box<DataType>, Box<DataType>),
    Tuple(Vec<(Option<String>, DataType)>),
    Nullable(Box<DataType>),
    /// Dictionary-encoded storage; a hint that the column holds few distinct values.
    LowCardinality(Box<DataType>),
    /// A type dbgen can parse but not generate (aggregate states, variants...). Such a
    /// column can only be left to its default or set to NULL.
    Unsupported(String),
}

impl DataType {
    pub fn is_nullable(&self) -> bool {
        match self {
            DataType::Nullable(_) => true,
            DataType::LowCardinality(inner) => inner.is_nullable(),
            _ => false,
        }
    }

    /// The type without `Nullable` and `LowCardinality` wrappers.
    pub fn base(&self) -> &DataType {
        match self {
            DataType::Nullable(inner) | DataType::LowCardinality(inner) => inner.base(),
            other => other,
        }
    }

    pub fn is_low_cardinality(&self) -> bool {
        match self {
            DataType::LowCardinality(_) => true,
            DataType::Nullable(inner) => inner.is_low_cardinality(),
            _ => false,
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self.base(), DataType::Int { .. })
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self.base(), DataType::Int { .. } | DataType::Float { .. } | DataType::Decimal { .. } | DataType::Bool)
    }

    pub fn is_temporal(&self) -> bool {
        matches!(self.base(), DataType::Date | DataType::Date32 | DataType::DateTime { .. })
    }

    pub fn is_textual(&self) -> bool {
        matches!(self.base(), DataType::String { .. } | DataType::FixedString(_))
    }

    /// Whether the type is or contains `JSON`.
    pub fn contains_json(&self) -> bool {
        match self {
            DataType::Json => true,
            DataType::Nullable(t) | DataType::LowCardinality(t) | DataType::Array(t) => t.contains_json(),
            DataType::Map(k, v) => k.contains_json() || v.contains_json(),
            DataType::Tuple(items) => items.iter().any(|(_, t)| t.contains_json()),
            _ => false,
        }
    }

    /// The part of the type dbgen cannot generate, if any.
    pub fn unsupported(&self) -> Option<&str> {
        match self {
            DataType::Unsupported(s) => Some(s),
            DataType::Nullable(t) | DataType::LowCardinality(t) | DataType::Array(t) => t.unsupported(),
            DataType::Map(k, v) => k.unsupported().or_else(|| v.unsupported()),
            DataType::Tuple(items) => items.iter().find_map(|(_, t)| t.unsupported()),
            _ => None,
        }
    }

    /// Inclusive range of an integer type, clamped to `i128`.
    pub fn int_range(&self) -> Option<(i128, i128)> {
        match self.base() {
            DataType::Int { bits, signed } => Some(int_range(*bits, *signed)),
            DataType::Bool => Some((0, 1)),
            _ => None,
        }
    }

    /// Number of distinct non-null values, when small enough to matter (`None` = huge).
    pub fn cardinality(&self) -> Option<u128> {
        match self.base() {
            DataType::Bool => Some(2),
            DataType::Enum { variants, .. } => Some(variants.len() as u128),
            DataType::Int { bits, .. } if *bits <= 64 => Some(1u128 << bits),
            DataType::FixedString(n) if *n <= 8 => Some(1u128 << (8 * n)),
            DataType::Date => Some(65_536),
            _ => None,
        }
    }
}

/// Inclusive range of an integer of `bits` bits, clamped to `i128`.
pub fn int_range(bits: u16, signed: bool) -> (i128, i128) {
    match (bits, signed) {
        (b, true) if b < 128 => (-(1i128 << (b - 1)), (1i128 << (b - 1)) - 1),
        (b, false) if b < 128 => (0, (1i128 << b) - 1),
        (_, true) => (i128::MIN, i128::MAX),
        (_, false) => (0, i128::MAX),
    }
}

/// Renders in ClickHouse syntax, which is also the most explicit of the supported dialects.
impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Bool => write!(f, "Bool"),
            DataType::Int { bits, signed } => write!(f, "{}Int{bits}", if *signed { "" } else { "U" }),
            DataType::Float { bits } => write!(f, "Float{bits}"),
            DataType::Decimal { precision, scale } => write!(f, "Decimal({precision}, {scale})"),
            DataType::String { max_len: None } => write!(f, "String"),
            DataType::String { max_len: Some(n) } => write!(f, "String({n})"),
            DataType::FixedString(n) => write!(f, "FixedString({n})"),
            DataType::Uuid => write!(f, "UUID"),
            DataType::Date => write!(f, "Date"),
            DataType::Date32 => write!(f, "Date32"),
            DataType::DateTime { precision: None, tz: None } => write!(f, "DateTime"),
            DataType::DateTime { precision: None, tz: Some(tz) } => write!(f, "DateTime('{tz}')"),
            DataType::DateTime { precision: Some(p), tz: None } => write!(f, "DateTime64({p})"),
            DataType::DateTime { precision: Some(p), tz: Some(tz) } => write!(f, "DateTime64({p}, '{tz}')"),
            DataType::Enum { bits, variants } => {
                let items: Vec<String> = variants.iter().map(|(n, v)| format!("'{}' = {v}", n.replace('\'', "\\'"))).collect();
                write!(f, "Enum{bits}({})", items.join(", "))
            }
            DataType::Ipv4 => write!(f, "IPv4"),
            DataType::Ipv6 => write!(f, "IPv6"),
            DataType::Json => write!(f, "JSON"),
            DataType::Array(t) => write!(f, "Array({t})"),
            DataType::Map(k, v) => write!(f, "Map({k}, {v})"),
            DataType::Tuple(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|(n, t)| match n {
                        Some(n) => format!("{n} {t}"),
                        None => t.to_string(),
                    })
                    .collect();
                write!(f, "Tuple({})", parts.join(", "))
            }
            DataType::Nullable(t) => write!(f, "Nullable({t})"),
            DataType::LowCardinality(t) => write!(f, "LowCardinality({t})"),
            DataType::Unsupported(s) => write!(f, "{s}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers() {
        let t = DataType::LowCardinality(Box::new(DataType::Nullable(Box::new(DataType::String { max_len: None }))));
        assert!(t.is_nullable());
        assert!(t.is_low_cardinality());
        assert_eq!(t.base(), &DataType::String { max_len: None });
        assert_eq!(t.to_string(), "LowCardinality(Nullable(String))");
        assert_eq!(int_range(8, true), (-128, 127));
        assert_eq!(int_range(16, false), (0, 65_535));
    }
}
