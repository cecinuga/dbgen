//! ClickHouse over its HTTP interface: queries as text, inserts and reads in `RowBinary`,
//! encoded from the column types of the compiled schema.

use std::collections::HashMap;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::schema::{DataType, Table};
use crate::value::{parse_uuid, Num, Value};

use super::DbError;

#[derive(Debug, Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    user: String,
    password: String,
    pub database: String,
    pub host: String,
    pub port: u16,
}

/// Server error codes worth retrying: the request was refused before any row was written,
/// or the server was momentarily unable to serve it.
const TRANSIENT_CODES: &[u32] = &[159, 164, 202, 209, 210, 241, 242, 252, 319, 425, 999];

impl Client {
    /// `http(s)://user:password@host:port/database` or `clickhouse://...` (HTTP, port 8123
    /// by default; `?secure=true` or port 8443 for HTTPS).
    pub fn connect(url: &str, timeout_secs: u64) -> Result<Client> {
        let bad = |m: &str| Error::Config(format!("database URL: {m}"));
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("expected scheme://host"))?;
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, q),
            None => (rest, ""),
        };
        let secure_param = query.split('&').any(|kv| kv.eq_ignore_ascii_case("secure=true") || kv.eq_ignore_ascii_case("secure=1"));
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, p),
            None => (rest, ""),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((a, b)) => (percent_decode(a), percent_decode(b)),
                None => (percent_decode(u), String::new()),
            },
            None => ("default".to_string(), String::new()),
        };
        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            let (h, p) = stripped.split_once(']').ok_or_else(|| bad("unterminated [IPv6] host"))?;
            (format!("[{h}]"), p.strip_prefix(':').map(str::to_string))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p.to_string())),
                None => (hostport.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let scheme = scheme.to_ascii_lowercase();
        let mut https = scheme == "https" || scheme == "clickhouses" || secure_param;
        let port: u16 = match port {
            Some(p) => p.parse().map_err(|_| bad(&format!("bad port '{p}'")))?,
            None if https => 8443,
            None => 8123,
        };
        if port == 9000 || port == 9440 {
            return Err(bad(&format!(
                "port {port} is ClickHouse's native protocol; dbgen uses the HTTP interface (8123, or 8443 with TLS)"
            )));
        }
        if port == 8443 {
            https = true;
        }
        let database = match percent_decode(path.trim_end_matches('/')) {
            d if d.is_empty() => "default".to_string(),
            d => d,
        };
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(timeout_secs)).build();
        let base = format!("{}://{host}:{port}/", if https { "https" } else { "http" });
        Ok(Client { agent, base, user, password, database, host, port })
    }

    pub fn describe(&self) -> String {
        format!("clickhouse {}:{}/{}", self.host, self.port, self.database)
    }

    fn post(&self, params: &[(&str, &str)], body: &[u8]) -> std::result::Result<Vec<u8>, DbError> {
        let mut url = self.base.clone();
        url.push_str("?database=");
        url.push_str(&percent_encode(&self.database));
        for (k, v) in params {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(&percent_encode(v));
        }
        let req = self.agent.post(&url).set("X-ClickHouse-User", &self.user).set("X-ClickHouse-Key", &self.password);
        match req.send_bytes(body) {
            Ok(resp) => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).map_err(|e| DbError::transient(format!("reading the response: {e}")))?;
                Ok(out)
            }
            Err(ureq::Error::Status(status, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                let code = text.strip_prefix("Code: ").and_then(|r| r.split('.').next()).and_then(|c| c.trim().parse::<u32>().ok());
                let transient = status == 503 || code.is_some_and(|c| TRANSIENT_CODES.contains(&c));
                let message = text.trim().lines().next().unwrap_or("").to_string();
                Err(DbError { message: format!("HTTP {status}: {message}"), transient, connect: false })
            }
            Err(ureq::Error::Transport(t)) => {
                let connect = matches!(t.kind(), ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns);
                Err(DbError { message: t.to_string(), transient: true, connect })
            }
        }
    }

    /// Raw response bytes (for binary formats).
    pub fn query_bytes(&self, sql: &str) -> std::result::Result<Vec<u8>, DbError> {
        self.post(&[], sql.as_bytes())
    }

    pub fn query_text(&self, sql: &str) -> std::result::Result<String, DbError> {
        self.post(&[], sql.as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn execute(&self, sql: &str) -> std::result::Result<(), DbError> {
        self.post(&[], sql.as_bytes()).map(|_| ())
    }

    pub fn quote(name: &str) -> String {
        format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
    }

    pub fn qualified(&self, table: &str) -> String {
        format!("{}.{}", Client::quote(&self.database), Client::quote(table))
    }

    pub fn version(&self) -> std::result::Result<String, DbError> {
        self.query_text("SELECT version() FORMAT TSVRaw").map(|s| s.trim().to_string())
    }

    /// Table -> its columns and types, as the server reports them.
    pub fn live_columns(&self) -> std::result::Result<HashMap<String, Vec<(String, String)>>, DbError> {
        let sql = format!(
            "SELECT table, name, type FROM system.columns WHERE database = '{}' ORDER BY table, position FORMAT TSVRaw",
            self.database.replace('\\', "\\\\").replace('\'', "\\'")
        );
        let mut out: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for line in self.query_text(&sql)?.lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(t), Some(c), Some(ty)) = (parts.next(), parts.next(), parts.next()) {
                out.entry(t.to_string()).or_default().push((c.to_string(), ty.to_string()));
            }
        }
        Ok(out)
    }

    pub fn query_u64(&self, sql: &str) -> std::result::Result<u64, DbError> {
        let text = self.query_text(&format!("{sql} FORMAT TSVRaw"))?;
        text.trim().parse().map_err(|_| DbError { message: format!("unexpected result '{}' for: {sql}", text.trim()), transient: false, connect: false })
    }

    pub fn count(&self, table: &str) -> std::result::Result<u64, DbError> {
        self.query_u64(&format!("SELECT count() FROM {}", self.qualified(table)))
    }

    pub fn truncate(&self, table: &str) -> std::result::Result<(), DbError> {
        self.execute(&format!("TRUNCATE TABLE {} SYNC", self.qualified(table)))
    }

    pub fn insert(&self, table: &str, columns: &[&str], body: &[u8], json_as_string: bool) -> std::result::Result<(), DbError> {
        let cols: Vec<String> = columns.iter().map(|c| Client::quote(c)).collect();
        let query = format!("INSERT INTO {} ({}) FORMAT RowBinary", self.qualified(table), cols.join(", "));
        let mut params: Vec<(&str, &str)> = vec![("query", &query), ("async_insert", "0")];
        if json_as_string {
            params.push(("input_format_binary_read_json_as_string", "1"));
        }
        self.post(&params, body).map(|_| ())
    }

    /// Up to `limit` rows of `columns`, sorted, so the result is deterministic.
    pub fn read_rows(&self, table: &Table, columns: &[usize], limit: u64) -> std::result::Result<Vec<Vec<Value>>, DbError> {
        let names: Vec<String> = columns.iter().map(|&c| Client::quote(&table.columns[c].name)).collect();
        let types: Vec<&DataType> = columns.iter().map(|&c| &table.columns[c].ty).collect();
        let list = names.join(", ");
        let fin = if table.collapsing { " FINAL" } else { "" };
        let sql = format!(
            "SELECT DISTINCT {list} FROM {}{fin} ORDER BY {list} LIMIT {limit} FORMAT RowBinary",
            self.qualified(&table.name)
        );
        let mut params: Vec<(&str, &str)> = Vec::new();
        if types.iter().any(|t| t.contains_json()) {
            params.push(("output_format_binary_write_json_as_string", "1"));
        }
        let bytes = self.post(&params, sql.as_bytes())?;
        let mut cur = Cursor { buf: &bytes, pos: 0 };
        let mut rows = Vec::new();
        while cur.pos < bytes.len() {
            let mut row = Vec::with_capacity(types.len());
            for t in &types {
                row.push(decode(&mut cur, t).map_err(|m| DbError { message: format!("decoding {}: {m}", table.name), transient: false, connect: false })?);
            }
            rows.push(row);
        }
        Ok(rows)
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// RowBinary
// ---------------------------------------------------------------------------------------

fn leb128(buf: &mut Vec<u8>, mut n: u64) {
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn int_bytes(buf: &mut Vec<u8>, v: i128, bits: u16) {
    match bits {
        8 => buf.push(v as u8),
        16 => buf.extend_from_slice(&(v as u16).to_le_bytes()),
        32 => buf.extend_from_slice(&(v as u32).to_le_bytes()),
        64 => buf.extend_from_slice(&(v as u64).to_le_bytes()),
        128 => buf.extend_from_slice(&(v as u128).to_le_bytes()),
        _ => {
            buf.extend_from_slice(&(v as u128).to_le_bytes());
            let ext = if v < 0 { 0xFF } else { 0 };
            buf.extend(std::iter::repeat_n(ext, (bits as usize / 8).saturating_sub(16)));
        }
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => other.to_string().trim_matches('\'').to_string(),
    }
}

/// Appends `v` to `buf` in RowBinary for a column of type `ty`.
pub fn encode(buf: &mut Vec<u8>, ty: &DataType, v: &Value) -> std::result::Result<(), String> {
    match ty {
        DataType::Nullable(inner) => {
            if v.is_null() {
                buf.push(1);
                return Ok(());
            }
            buf.push(0);
            return encode(buf, inner, v);
        }
        DataType::LowCardinality(inner) => return encode(buf, inner, v),
        _ => {}
    }
    if v.is_null() {
        return Err(format!("NULL for non-nullable {ty}"));
    }
    match ty {
        DataType::Bool => buf.push(v.truth().ok_or("not a boolean")? as u8),
        DataType::Int { bits, signed } => {
            let x = match v {
                Value::Int(i) => *i,
                Value::Bool(b) => *b as i128,
                Value::Decimal(d, s) => Num::D(*d, *s).to_scaled(0).ok_or("decimal out of range")?,
                Value::Float(f) => f.round() as i128,
                other => return Err(format!("{} for {ty}", other.type_name())),
            };
            let (lo, hi) = crate::schema::types::int_range(*bits, *signed);
            if x < lo || x > hi {
                return Err(format!("{x} out of range for {ty}"));
            }
            int_bytes(buf, x, *bits);
        }
        DataType::Float { bits } => {
            let f = v.as_f64().ok_or_else(|| format!("{} for {ty}", v.type_name()))?;
            if *bits == 32 {
                buf.extend_from_slice(&(f as f32).to_le_bytes());
            } else {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        DataType::Decimal { precision, scale } => {
            let x = v.num().and_then(|n| n.to_scaled(*scale)).ok_or_else(|| format!("{} for {ty}", v.type_name()))?;
            let bits = match precision {
                0..=9 => 32,
                10..=18 => 64,
                19..=38 => 128,
                _ => 256,
            };
            int_bytes(buf, x, bits);
        }
        DataType::String { .. } | DataType::Json => {
            let s = text(v);
            leb128(buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        DataType::FixedString(n) => {
            let s = text(v);
            let bytes = s.as_bytes();
            let n = *n as usize;
            buf.extend_from_slice(&bytes[..bytes.len().min(n)]);
            buf.extend(std::iter::repeat_n(0u8, n.saturating_sub(bytes.len())));
        }
        DataType::Uuid => {
            let u = match v {
                Value::Uuid(u) => *u,
                Value::Str(s) => parse_uuid(s).ok_or("not a UUID")?,
                other => return Err(format!("{} for UUID", other.type_name())),
            };
            buf.extend_from_slice(&((u >> 64) as u64).to_le_bytes());
            buf.extend_from_slice(&(u as u64).to_le_bytes());
        }
        DataType::Date => {
            let d = days(v)?;
            if !(0..=65_535).contains(&d) {
                return Err(format!("day {d} out of range for Date"));
            }
            buf.extend_from_slice(&(d as u16).to_le_bytes());
        }
        DataType::Date32 => buf.extend_from_slice(&(days(v)? as i32).to_le_bytes()),
        DataType::DateTime { precision: None, .. } => {
            let secs = v.as_time().ok_or_else(|| format!("{} for {ty}", v.type_name()))?.div_euclid(1_000_000);
            if !(0..=u32::MAX as i64).contains(&secs) {
                return Err(format!("{secs} s out of range for DateTime"));
            }
            buf.extend_from_slice(&(secs as u32).to_le_bytes());
        }
        DataType::DateTime { precision: Some(p), .. } => {
            let us = v.as_time().ok_or_else(|| format!("{} for {ty}", v.type_name()))?;
            let ticks = if *p <= 6 { us.div_euclid(10i64.pow(6 - *p as u32)) } else { us.saturating_mul(10i64.pow(*p as u32 - 6)) };
            buf.extend_from_slice(&ticks.to_le_bytes());
        }
        DataType::Enum { bits, variants } => {
            let name = text(v);
            let (_, value) = variants.iter().find(|(n, _)| *n == name).ok_or_else(|| format!("'{name}' is not a value of {ty}"))?;
            if *bits == 8 {
                buf.push(*value as i8 as u8);
            } else {
                buf.extend_from_slice(&value.to_le_bytes());
            }
        }
        DataType::Ipv4 => {
            let ip = match v {
                Value::Ip4(ip) => *ip,
                Value::Str(s) => u32::from(s.parse::<std::net::Ipv4Addr>().map_err(|e| e.to_string())?),
                other => return Err(format!("{} for IPv4", other.type_name())),
            };
            buf.extend_from_slice(&ip.to_le_bytes());
        }
        DataType::Ipv6 => {
            let ip = match v {
                Value::Ip6(ip) => *ip,
                Value::Ip4(ip) => 0xFFFF_0000_0000u128 | *ip as u128,
                Value::Str(s) => u128::from(s.parse::<std::net::Ipv6Addr>().map_err(|e| e.to_string())?),
                other => return Err(format!("{} for IPv6", other.type_name())),
            };
            buf.extend_from_slice(&ip.to_be_bytes());
        }
        DataType::Array(t) => {
            let Value::Array(items) = v else { return Err(format!("{} for {ty}", v.type_name())) };
            leb128(buf, items.len() as u64);
            for item in items {
                encode(buf, t, item)?;
            }
        }
        DataType::Map(k, t) => {
            let Value::Map(entries) = v else { return Err(format!("{} for {ty}", v.type_name())) };
            leb128(buf, entries.len() as u64);
            for (key, val) in entries {
                encode(buf, k, key)?;
                encode(buf, t, val)?;
            }
        }
        DataType::Tuple(items) => {
            let Value::Tuple(vals) = v else { return Err(format!("{} for {ty}", v.type_name())) };
            if vals.len() != items.len() {
                return Err(format!("{} values for {ty}", vals.len()));
            }
            for ((_, t), val) in items.iter().zip(vals) {
                encode(buf, t, val)?;
            }
        }
        DataType::Unsupported(s) => return Err(format!("type {s} cannot be encoded")),
        DataType::Nullable(_) | DataType::LowCardinality(_) => unreachable!(),
    }
    Ok(())
}

fn days(v: &Value) -> std::result::Result<i64, String> {
    match v {
        Value::Date(d) => Ok(*d as i64),
        other => other.as_time().map(crate::timeutil::day_of).ok_or_else(|| format!("{} for a date", other.type_name())),
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> std::result::Result<&[u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or("truncated data")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> std::result::Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn leb128(&mut self) -> std::result::Result<u64, String> {
        let mut n = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            n |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
            shift += 7;
            if shift > 63 {
                return Err("bad length".into());
            }
        }
    }

    fn int(&mut self, bits: u16, signed: bool) -> std::result::Result<i128, String> {
        let bytes = self.take(bits as usize / 8)?;
        let mut le = [0u8; 16];
        let n = bytes.len().min(16);
        le[..n].copy_from_slice(&bytes[..n]);
        let negative = signed && bytes[bytes.len() - 1] & 0x80 != 0;
        if negative && n < 16 {
            le[n..].fill(0xFF);
        }
        Ok(i128::from_le_bytes(le))
    }
}

fn decode(cur: &mut Cursor, ty: &DataType) -> std::result::Result<Value, String> {
    Ok(match ty {
        DataType::Nullable(inner) => {
            if cur.u8()? == 1 {
                Value::Null
            } else {
                decode(cur, inner)?
            }
        }
        DataType::LowCardinality(inner) => decode(cur, inner)?,
        DataType::Bool => Value::Bool(cur.u8()? != 0),
        DataType::Int { bits, signed } => Value::Int(cur.int(*bits, *signed)?),
        DataType::Float { bits: 32 } => Value::Float(f32::from_le_bytes(cur.take(4)?.try_into().unwrap()) as f64),
        DataType::Float { .. } => Value::Float(f64::from_le_bytes(cur.take(8)?.try_into().unwrap())),
        DataType::Decimal { precision, scale } => {
            let bits = match precision {
                0..=9 => 32,
                10..=18 => 64,
                19..=38 => 128,
                _ => 256,
            };
            Value::Decimal(cur.int(bits, true)?, *scale)
        }
        DataType::String { .. } | DataType::Json => {
            let n = cur.leb128()? as usize;
            Value::Str(String::from_utf8_lossy(cur.take(n)?).into_owned())
        }
        DataType::FixedString(n) => Value::Str(String::from_utf8_lossy(cur.take(*n as usize)?).trim_end_matches('\0').to_string()),
        DataType::Uuid => {
            let hi = u64::from_le_bytes(cur.take(8)?.try_into().unwrap());
            let lo = u64::from_le_bytes(cur.take(8)?.try_into().unwrap());
            Value::Uuid(((hi as u128) << 64) | lo as u128)
        }
        DataType::Date => Value::Date(u16::from_le_bytes(cur.take(2)?.try_into().unwrap()) as i32),
        DataType::Date32 => Value::Date(i32::from_le_bytes(cur.take(4)?.try_into().unwrap())),
        DataType::DateTime { precision: None, .. } => Value::Time(u32::from_le_bytes(cur.take(4)?.try_into().unwrap()) as i64 * 1_000_000),
        DataType::DateTime { precision: Some(p), .. } => {
            let ticks = i64::from_le_bytes(cur.take(8)?.try_into().unwrap());
            Value::Time(if *p <= 6 { ticks * 10i64.pow(6 - *p as u32) } else { ticks / 10i64.pow(*p as u32 - 6) })
        }
        DataType::Enum { bits, variants } => {
            let v = if *bits == 8 { cur.u8()? as i8 as i16 } else { i16::from_le_bytes(cur.take(2)?.try_into().unwrap()) };
            Value::Str(variants.iter().find(|(_, x)| *x == v).map(|(n, _)| n.clone()).unwrap_or_default())
        }
        DataType::Ipv4 => Value::Ip4(u32::from_le_bytes(cur.take(4)?.try_into().unwrap())),
        DataType::Ipv6 => Value::Ip6(u128::from_be_bytes(cur.take(16)?.try_into().unwrap())),
        DataType::Array(t) => {
            let n = cur.leb128()?;
            Value::Array((0..n).map(|_| decode(cur, t)).collect::<std::result::Result<_, _>>()?)
        }
        DataType::Map(k, v) => {
            let n = cur.leb128()?;
            let mut entries = Vec::with_capacity(n as usize);
            for _ in 0..n {
                entries.push((decode(cur, k)?, decode(cur, v)?));
            }
            Value::Map(entries)
        }
        DataType::Tuple(items) => Value::Tuple(items.iter().map(|(_, t)| decode(cur, t)).collect::<std::result::Result<_, _>>()?),
        DataType::Unsupported(s) => return Err(format!("type {s} cannot be decoded")),
    })
}

// ---------------------------------------------------------------------------------------
// Verification queries
// ---------------------------------------------------------------------------------------

impl Client {
    /// Rows beyond the first for each value of `key` (0 = the key is unique).
    pub fn duplicates_sql(&self, table: &Table, key: &[String]) -> String {
        let fin = if table.collapsing { " FINAL" } else { "" };
        format!("SELECT count() - uniqExact(tuple({})) FROM {}{fin}", key.join(", "), self.qualified(&table.name))
    }

    /// Rows lost to merges: raw rows minus rows after deduplication.
    pub fn collapsed_sql(&self, table: &Table) -> String {
        let t = self.qualified(&table.name);
        format!("SELECT (SELECT count() FROM {t}) - (SELECT count() FROM {t} FINAL)")
    }

    /// Child rows whose (non-NULL) reference has no parent row.
    pub fn orphans_sql(&self, child: &Table, child_cols: &[&str], parent: &Table, parent_cols: &[&str]) -> String {
        let cc: Vec<String> = child_cols.iter().map(|c| Client::quote(c)).collect();
        let pc: Vec<String> = parent_cols.iter().map(|c| Client::quote(c)).collect();
        let not_null: Vec<String> = cc.iter().map(|c| format!("{c} IS NOT NULL")).collect();
        let lhs = if cc.len() == 1 { cc[0].clone() } else { format!("({})", cc.join(", ")) };
        format!(
            "SELECT count() FROM {} WHERE {} AND {lhs} NOT IN (SELECT {} FROM {})",
            self.qualified(&child.name),
            not_null.join(" AND "),
            pc.join(", "),
            self.qualified(&parent.name)
        )
    }

    pub fn violations_sql(&self, table: &Table, check_sql: &str) -> String {
        format!("SELECT count() FROM {} WHERE NOT ({check_sql})", self.qualified(&table.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let c = Client::connect("clickhouse://u:p%40ss@db.example:8123/dwh", 10).unwrap();
        assert_eq!((c.user.as_str(), c.password.as_str(), c.database.as_str(), c.port), ("u", "p@ss", "dwh", 8123));
        assert!(c.base.starts_with("http://"));
        let s = Client::connect("https://db.example/x", 10).unwrap();
        assert_eq!(s.port, 8443);
        assert!(Client::connect("clickhouse://h:9000/x", 10).unwrap_err().to_string().contains("native protocol"));
        let d = Client::connect("http://localhost", 10).unwrap();
        assert_eq!((d.user.as_str(), d.database.as_str()), ("default", "default"));
    }

    #[test]
    fn encodes_as_clickhouse_does() {
        // Byte layouts checked against ClickHouse 26.8 (`SELECT ... FORMAT RowBinary`).
        let mut b = Vec::new();
        encode(&mut b, &DataType::Uuid, &Value::Uuid(parse_uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0").unwrap())).unwrap();
        assert_eq!(b, [0xe7, 0x11, 0xb3, 0x5c, 0x04, 0xc4, 0xf0, 0x61, 0xa0, 0xdb, 0xd3, 0x6a, 0x00, 0xa6, 0x7b, 0x90]);
        b.clear();
        let e8 = DataType::Enum { bits: 8, variants: vec![("a".into(), 1), ("b".into(), -2)] };
        encode(&mut b, &e8, &Value::Str("b".into())).unwrap();
        encode(&mut b, &DataType::Decimal { precision: 9, scale: 3 }, &Value::Decimal(15, 1)).unwrap();
        encode(&mut b, &DataType::Date32, &Value::Date(-7305)).unwrap();
        encode(&mut b, &DataType::Ipv4, &Value::Ip4(0x0102_0304)).unwrap();
        assert_eq!(b, [0xfe, 0xdc, 0x05, 0, 0, 0x77, 0xe3, 0xff, 0xff, 0x04, 0x03, 0x02, 0x01]);
        b.clear();
        encode(&mut b, &DataType::Nullable(Box::new(DataType::String { max_len: None })), &Value::Str("hé".into())).unwrap();
        assert_eq!(b, [0, 3, b'h', 0xc3, 0xa9]);
        assert!(encode(&mut b, &DataType::Int { bits: 8, signed: false }, &Value::Int(256)).is_err());
        assert!(encode(&mut b, &DataType::String { max_len: None }, &Value::Null).is_err());
    }

    #[test]
    fn round_trip() {
        let types = [
            DataType::Int { bits: 256, signed: true },
            DataType::Int { bits: 64, signed: false },
            DataType::DateTime { precision: Some(9), tz: None },
            DataType::Decimal { precision: 40, scale: 2 },
            DataType::Map(Box::new(DataType::String { max_len: None }), Box::new(DataType::Array(Box::new(DataType::Nullable(Box::new(DataType::Int { bits: 16, signed: true })))))),
            DataType::FixedString(4),
            DataType::Ipv6,
        ];
        let values = [
            Value::Int(-5),
            Value::Int(u64::MAX as i128),
            Value::Time(1_700_000_000_123_456),
            Value::Decimal(-12_345, 2),
            Value::Map(vec![(Value::Str("k".into()), Value::Array(vec![Value::Int(-1), Value::Null]))]),
            Value::Str("ab".into()),
            Value::Ip6(1),
        ];
        let mut buf = Vec::new();
        for (t, v) in types.iter().zip(&values) {
            encode(&mut buf, t, v).unwrap();
        }
        let mut cur = Cursor { buf: &buf, pos: 0 };
        for (t, v) in types.iter().zip(&values) {
            assert_eq!(&decode(&mut cur, t).unwrap(), v, "{t}");
        }
        assert_eq!(cur.pos, buf.len());
    }
}
