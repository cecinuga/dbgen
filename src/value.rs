//! Values as the generator produces them and the expression evaluator reads them.
//!
//! The representation is dialect-neutral: a database backend converts a `Value` into its
//! wire format according to the column's [`DataType`](crate::schema::DataType).

use std::cmp::Ordering;
use std::fmt;

use crate::timeutil::{day_of, Rfc3339, Ymd, US_PER_DAY};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Any integer type up to 128 bits.
    Int(i128),
    Float(f64),
    /// Fixed-point: `unscaled / 10^scale`.
    Decimal(i128, u8),
    Str(String),
    Uuid(u128),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since the Unix epoch, UTC.
    Time(i64),
    Ip4(u32),
    Ip6(u128),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Tuple(Vec<Value>),
    /// A duration in microseconds; only produced by `INTERVAL` expressions.
    Interval(i64),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// SQL truth value: `None` for NULL, numbers are true when non-zero.
    pub fn truth(&self) -> Option<bool> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            Value::Float(f) => Some(*f != 0.0),
            Value::Decimal(d, _) => Some(*d != 0),
            _ => Some(true),
        }
    }

    pub fn num(&self) -> Option<Num> {
        match self {
            Value::Bool(b) => Some(Num::I(*b as i128)),
            Value::Int(i) => Some(Num::I(*i)),
            Value::Float(f) => Some(Num::F(*f)),
            Value::Decimal(d, s) => Some(Num::D(*d, *s)),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.num().map(Num::to_f64)
    }

    /// Instant in microseconds for dates and timestamps; parses string literals too.
    pub fn as_time(&self) -> Option<i64> {
        match self {
            Value::Time(t) => Some(*t),
            Value::Date(d) => Some(*d as i64 * US_PER_DAY),
            Value::Str(s) => crate::timeutil::parse_instant(s).ok(),
            _ => None,
        }
    }

    /// SQL comparison. `None` when either side is NULL or the types are not comparable.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        use Value::*;
        match (self, other) {
            (Null, _) | (_, Null) => None,
            (Str(a), Str(b)) => Some(a.cmp(b)),
            (Uuid(a), Uuid(b)) => Some(a.cmp(b)),
            (Uuid(a), Str(s)) => parse_uuid(s).map(|b| a.cmp(&b)),
            (Str(s), Uuid(b)) => parse_uuid(s).map(|a| a.cmp(b)),
            (Ip4(a), Ip4(b)) => Some(a.cmp(b)),
            (Ip6(a), Ip6(b)) => Some(a.cmp(b)),
            (Date(a), Date(b)) => Some(a.cmp(b)),
            (Time(_) | Date(_), Time(_) | Date(_) | Str(_)) | (Str(_), Time(_) | Date(_)) => {
                Some(self.as_time()?.cmp(&other.as_time()?))
            }
            (Interval(a), Interval(b)) => Some(a.cmp(b)),
            (Array(a), Array(b)) | (Tuple(a), Tuple(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.compare(y)? {
                        Ordering::Equal => continue,
                        o => return Some(o),
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => Num::compare(self.num()?, other.num()?),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::Decimal(..) => "decimal",
            Value::Str(_) => "string",
            Value::Uuid(_) => "UUID",
            Value::Date(_) => "date",
            Value::Time(_) => "timestamp",
            Value::Ip4(_) => "IPv4",
            Value::Ip6(_) => "IPv6",
            Value::Array(_) => "array",
            Value::Map(_) => "map",
            Value::Tuple(_) => "tuple",
            Value::Interval(_) => "interval",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Decimal(d, s) => write!(f, "{}", format_decimal(*d, *s)),
            Value::Str(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Value::Uuid(u) => write!(f, "'{}'", format_uuid(*u)),
            Value::Date(d) => write!(f, "'{}'", Ymd(*d as i64)),
            Value::Time(t) => write!(f, "'{}'", Rfc3339(*t)),
            Value::Ip4(ip) => write!(f, "'{}'", std::net::Ipv4Addr::from(*ip)),
            Value::Ip6(ip) => write!(f, "'{}'", std::net::Ipv6Addr::from(*ip)),
            Value::Interval(us) => write!(f, "INTERVAL {us} MICROSECOND"),
            Value::Array(items) => write!(f, "[{}]", join(items)),
            Value::Tuple(items) => write!(f, "({})", join(items)),
            Value::Map(entries) => {
                let parts: Vec<String> = entries.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "{{{}}}", parts.join(", "))
            }
        }
    }
}

fn join(items: &[Value]) -> String {
    items.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")
}

pub fn format_decimal(unscaled: i128, scale: u8) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let p = 10i128.pow(scale as u32);
    let sign = if unscaled < 0 { "-" } else { "" };
    let a = unscaled.unsigned_abs();
    format!("{sign}{}.{:0width$}", a / p as u128, a % p as u128, width = scale as usize)
}

pub fn format_uuid(u: u128) -> String {
    let h = format!("{u:032x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub fn parse_uuid(s: &str) -> Option<u128> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// Day index of a timestamp value.
pub fn date_of_time(us: i64) -> i32 {
    day_of(us) as i32
}

/// Numeric tower for evaluation: integers and decimals stay exact, anything mixed with a
/// float becomes a float. Decimal arithmetic matters: `a + b + c = 1` over `Decimal(4, 3)`
/// columns must hold exactly, as the database checks it exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    I(i128),
    D(i128, u8),
    F(f64),
}

impl Num {
    pub fn to_f64(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::D(d, s) => d as f64 / 10f64.powi(s as i32),
            Num::F(f) => f,
        }
    }

    /// Both operands as decimals of a common scale, when neither is a float.
    fn align(a: Num, b: Num) -> Option<(i128, i128, u8)> {
        let (da, sa) = match a {
            Num::I(i) => (i, 0),
            Num::D(d, s) => (d, s),
            Num::F(_) => return None,
        };
        let (db, sb) = match b {
            Num::I(i) => (i, 0),
            Num::D(d, s) => (d, s),
            Num::F(_) => return None,
        };
        let s = sa.max(sb);
        let up = |v: i128, from: u8| v.checked_mul(10i128.checked_pow((s - from) as u32)?);
        Some((up(da, sa)?, up(db, sb)?, s))
    }

    pub fn compare(a: Num, b: Num) -> Option<Ordering> {
        match Num::align(a, b) {
            Some((x, y, _)) => Some(x.cmp(&y)),
            None => a.to_f64().partial_cmp(&b.to_f64()),
        }
    }

    fn from_scaled(v: i128, s: u8) -> Num {
        if s == 0 {
            Num::I(v)
        } else {
            Num::D(v, s)
        }
    }

    pub fn add(a: Num, b: Num) -> Option<Num> {
        match Num::align(a, b) {
            Some((x, y, s)) => Some(Num::from_scaled(x.checked_add(y)?, s)),
            None => Some(Num::F(a.to_f64() + b.to_f64())),
        }
    }

    pub fn sub(a: Num, b: Num) -> Option<Num> {
        match Num::align(a, b) {
            Some((x, y, s)) => Some(Num::from_scaled(x.checked_sub(y)?, s)),
            None => Some(Num::F(a.to_f64() - b.to_f64())),
        }
    }

    pub fn mul(a: Num, b: Num) -> Option<Num> {
        match (a, b) {
            (Num::I(x), Num::I(y)) => Some(Num::I(x.checked_mul(y)?)),
            (Num::D(x, s), Num::I(y)) | (Num::I(y), Num::D(x, s)) => Some(Num::D(x.checked_mul(y)?, s)),
            (Num::D(x, sx), Num::D(y, sy)) if sx + sy <= 30 => Some(Num::D(x.checked_mul(y)?, sx + sy)),
            _ => Some(Num::F(a.to_f64() * b.to_f64())),
        }
    }

    /// Division is done in floating point, as SQL engines mostly do for `/`.
    pub fn div(a: Num, b: Num) -> Option<Num> {
        let d = b.to_f64();
        if d == 0.0 {
            return None;
        }
        Some(Num::F(a.to_f64() / d))
    }

    pub fn rem(a: Num, b: Num) -> Option<Num> {
        match (a, b) {
            (Num::I(x), Num::I(y)) if y != 0 => Some(Num::I(x % y)),
            _ => {
                let d = b.to_f64();
                if d == 0.0 {
                    None
                } else {
                    Some(Num::F(a.to_f64() % d))
                }
            }
        }
    }

    pub fn neg(self) -> Option<Num> {
        Some(match self {
            Num::I(i) => Num::I(i.checked_neg()?),
            Num::D(d, s) => Num::D(d.checked_neg()?, s),
            Num::F(f) => Num::F(-f),
        })
    }

    pub fn into_value(self) -> Value {
        match self {
            Num::I(i) => Value::Int(i),
            Num::D(d, s) => Value::Decimal(d, s),
            Num::F(f) => Value::Float(f),
        }
    }

    /// Rescales to `scale` decimal places, rounding half away from zero.
    pub fn to_scaled(self, scale: u8) -> Option<i128> {
        match self {
            Num::I(i) => i.checked_mul(10i128.checked_pow(scale as u32)?),
            Num::D(d, s) if s <= scale => d.checked_mul(10i128.checked_pow((scale - s) as u32)?),
            Num::D(d, s) => {
                let div = 10i128.checked_pow((s - scale) as u32)?;
                let q = d / div;
                let r = d % div;
                Some(if r.abs() * 2 >= div { q + d.signum() } else { q })
            }
            Num::F(f) => {
                let v = (f * 10f64.powi(scale as i32)).round();
                if v.is_finite() && v.abs() < 1.7e38 {
                    Some(v as i128)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_arithmetic_is_exact() {
        let a = Num::D(100, 3); // 0.100
        let b = Num::D(700, 3); // 0.700
        let c = Num::D(200, 3); // 0.200
        let sum = Num::add(Num::add(a, b).unwrap(), c).unwrap();
        assert_eq!(Num::compare(sum, Num::I(1)), Some(Ordering::Equal));
        assert_eq!(Num::to_scaled(Num::F(0.1255), 3), Some(126));
        assert_eq!(Num::to_scaled(Num::D(-1255, 4), 3), Some(-126));
        assert_eq!(format_decimal(-1500, 3), "-1.500");
        assert_eq!(format_decimal(5, 3), "0.005");
    }

    #[test]
    fn comparisons_coerce() {
        assert_eq!(Value::Int(2).compare(&Value::Decimal(1500, 3)), Some(Ordering::Greater));
        assert_eq!(Value::Bool(true).compare(&Value::Int(1)), Some(Ordering::Equal));
        assert_eq!(Value::Time(0).compare(&Value::Str("1970-01-01 00:00:01".into())), Some(Ordering::Less));
        assert_eq!(Value::Date(1).compare(&Value::Time(US_PER_DAY)), Some(Ordering::Equal));
        assert_eq!(Value::Null.compare(&Value::Int(1)), None);
        let u = parse_uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0").unwrap();
        assert_eq!(format_uuid(u), "61f0c404-5cb3-11e7-907b-a6006ad3dba0");
    }
}
