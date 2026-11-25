//! What a column's values are drawn from: random domains, unique-by-construction
//! sequences, and the name-based heuristics that pick plausible defaults (an `email`
//! column gets e-mail addresses, a `*_sec` column durations, a `birth_date` past dates).

use crate::expr::CmpOp;
use crate::rng::{mix, Rng};
use crate::schema::DataType;
use crate::timeutil::{days_from_civil, day_of, US_PER_DAY, US_PER_SEC};
use crate::value::{format_uuid, Num, Value};

use super::perm::Perm;
use super::words::*;

/// What a text column holds, which decides how its values look.
#[derive(Debug, Clone, PartialEq)]
pub enum TextKind {
    FirstName,
    LastName,
    FullName,
    Email,
    Phone,
    City,
    Country,
    Address,
    PostalCode,
    Url,
    Sentence,
    Word,
    Code,
    /// `"<Stem> <n>"`, e.g. `Floor 3` for `floor_name`.
    Label(String),
    /// `"<NAME>_<k>"` from a small pool, for status/type/category-like columns.
    Category(String),
    Uuid,
    Json,
    Ip,
}

impl TextKind {
    /// A kind named in the configuration.
    pub fn parse(s: &str) -> Option<TextKind> {
        Some(match s.to_ascii_lowercase().as_str() {
            "first_name" => TextKind::FirstName,
            "last_name" => TextKind::LastName,
            "full_name" | "name" => TextKind::FullName,
            "email" => TextKind::Email,
            "phone" => TextKind::Phone,
            "city" => TextKind::City,
            "country" => TextKind::Country,
            "address" => TextKind::Address,
            "postal_code" | "zip" => TextKind::PostalCode,
            "url" => TextKind::Url,
            "sentence" | "text" => TextKind::Sentence,
            "word" => TextKind::Word,
            "code" => TextKind::Code,
            "uuid" => TextKind::Uuid,
            "json" => TextKind::Json,
            "ip" => TextKind::Ip,
            _ => return None,
        })
    }

    /// The kind a column's name suggests.
    pub fn infer(table: &str, column: &str) -> TextKind {
        let col = column.to_ascii_lowercase();
        let tokens: Vec<&str> = col.split(['_', '.']).filter(|t| !t.is_empty()).collect();
        let has = |w: &str| tokens.contains(&w);
        let last = tokens.last().copied().unwrap_or("");
        if has("email") || has("mail") {
            return TextKind::Email;
        }
        if col.contains("first_name") || col == "firstname" || has("given") || col == "nome" {
            return TextKind::FirstName;
        }
        if col.contains("last_name") || col == "lastname" || has("surname") || col.contains("family_name") || col == "cognome" {
            return TextKind::LastName;
        }
        if col.contains("full_name") || col == "fullname" || col.contains("display_name") || col.contains("contact_name") {
            return TextKind::FullName;
        }
        if has("phone") || has("mobile") || has("tel") || has("telephone") || has("fax") {
            return TextKind::Phone;
        }
        if has("city") || has("town") || has("comune") {
            return TextKind::City;
        }
        if has("country") || has("nation") || has("nationality") {
            return TextKind::Country;
        }
        if has("address") || has("street") {
            return TextKind::Address;
        }
        if has("zip") || has("postal") || has("postcode") || has("cap") {
            return TextKind::PostalCode;
        }
        if has("url") || has("website") || has("link") || has("uri") || has("href") {
            return TextKind::Url;
        }
        if has("json") || has("payload") {
            return TextKind::Json;
        }
        if has("ip") {
            return TextKind::Ip;
        }
        if has("uuid") || has("guid") || last == "id" {
            return TextKind::Uuid;
        }
        if ["description", "desc", "notes", "note", "comment", "comments", "text", "message", "details", "reason", "remarks", "summary", "error", "body"]
            .iter()
            .any(|w| has(w))
        {
            return TextKind::Sentence;
        }
        if has("code") || has("sku") {
            return TextKind::Code;
        }
        if [
            "status", "state", "type", "kind", "category", "level", "severity", "priority", "gender", "sex", "role", "mode", "source",
            "channel", "resource", "lang", "language", "currency", "unit", "stage", "phase", "tier", "class", "group", "flag",
        ]
        .contains(&last)
        {
            return TextKind::Category(col.to_ascii_uppercase());
        }
        if last == "name" || last == "title" || last == "label" {
            let stem = if tokens.len() > 1 { tokens[..tokens.len() - 1].join(" ") } else { table_stem(table) };
            return TextKind::Label(capitalize(&stem));
        }
        TextKind::Word
    }

    /// A random value of this kind. With `pool`, values come from that many distinct
    /// ones (low-cardinality columns).
    pub fn render(&self, rng: &mut Rng, pool: Option<u32>) -> String {
        match self {
            TextKind::FirstName => rng.pick(FIRST_NAMES).to_string(),
            TextKind::LastName => rng.pick(LAST_NAMES).to_string(),
            TextKind::FullName => format!("{} {}", rng.pick(FIRST_NAMES), rng.pick(LAST_NAMES)),
            TextKind::Email => format!("{}.{}{}@example.org", ascii(rng.pick(FIRST_NAMES)), ascii(rng.pick(LAST_NAMES)), rng.below(100)),
            TextKind::Phone => format!("+39 3{:02} {:03} {:04}", rng.below(100), rng.below(1000), rng.below(10_000)),
            TextKind::City => rng.pick(CITIES).to_string(),
            TextKind::Country => rng.pick(COUNTRIES).to_string(),
            TextKind::Address => format!("{} {}", rng.pick(STREETS), 1 + rng.below(200)),
            TextKind::PostalCode => format!("{:05}", rng.below(100_000)),
            TextKind::Url => format!("https://example.org/{}/{}", rng.pick(WORDS), rng.below(100_000)),
            TextKind::Sentence => {
                let n = 3 + rng.below(10) as usize;
                let mut s = rng.pick(SENTENCE_STARTS).to_string();
                for _ in 0..n {
                    s.push(' ');
                    s.push_str(rng.pick(WORDS));
                }
                s.push('.');
                s
            }
            TextKind::Word => match pool {
                Some(p) => pooled_word(rng.below(p.max(1) as u64) as usize),
                None => format!("{}_{}", rng.pick(WORDS), rng.pick(WORDS)),
            },
            TextKind::Code => {
                let a = (b'A' + rng.below(26) as u8) as char;
                let b = (b'A' + rng.below(26) as u8) as char;
                format!("{a}{b}-{:04}", rng.below(10_000))
            }
            TextKind::Label(stem) => format!("{stem} {}", 1 + rng.below(pool.unwrap_or(100).max(1) as u64)),
            TextKind::Category(name) => format!("{name}_{}", 1 + rng.below(pool.unwrap_or(5).max(1) as u64)),
            TextKind::Uuid => format_uuid(rng.uuid_v4()),
            TextKind::Json => format!("{{\"id\": {}, \"value\": \"{}\"}}", rng.below(100_000), rng.pick(WORDS)),
            TextKind::Ip => format!("10.{}.{}.{}", rng.below(256), rng.below(256), 1 + rng.below(254)),
        }
    }

    /// A value distinct for every `i`.
    pub fn render_unique(&self, rng: &mut Rng, i: u64) -> String {
        let n = i + 1;
        match self {
            TextKind::Email => format!("{}.{}.{n}@example.org", ascii(rng.pick(FIRST_NAMES)), ascii(rng.pick(LAST_NAMES))),
            TextKind::Phone => format!("+39 3{:09}", i % 1_000_000_000),
            TextKind::Url => format!("https://example.org/{}/{n}", rng.pick(WORDS)),
            TextKind::Code => format!("C-{n:06}"),
            TextKind::Label(stem) => format!("{stem} {n}"),
            TextKind::Category(name) => format!("{name}_{n}"),
            TextKind::Uuid => format_uuid(unique_uuid(i, 0x5eed)),
            TextKind::Ip => {
                let x = 0x0A00_0000u32.wrapping_add(i as u32 + 1);
                std::net::Ipv4Addr::from(x).to_string()
            }
            TextKind::Json => format!("{{\"id\": {n}}}"),
            other => format!("{} #{n}", other.render(rng, None)),
        }
    }
}

fn pooled_word(k: usize) -> String {
    if k < WORDS.len() {
        WORDS[k].to_string()
    } else {
        format!("{}_{}", WORDS[k % WORDS.len()], k / WORDS.len())
    }
}

fn ascii(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            'à' | 'á' | 'â' | 'ä' => Some('a'),
            'è' | 'é' | 'ê' | 'ë' => Some('e'),
            'ì' | 'í' | 'î' | 'ï' => Some('i'),
            'ò' | 'ó' | 'ô' | 'ö' => Some('o'),
            'ù' | 'ú' | 'û' | 'ü' => Some('u'),
            'ç' => Some('c'),
            c if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `dim_room_types` -> `room type`.
pub fn table_stem(table: &str) -> String {
    let t = strip_table_prefix(&table.to_ascii_lowercase());
    singular(&t).replace('_', " ")
}

/// A table name without its warehouse prefix: `dim_staff` -> `staff`.
pub fn strip_table_prefix(t: &str) -> String {
    for p in ["dim_", "fact_", "fct_", "d_", "f_", "tbl_", "t_", "ref_", "lkp_", "lookup_", "stg_"] {
        if let Some(rest) = t.strip_prefix(p) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    t.to_string()
}

/// The English singular of a plural noun, as far as table names need it.
pub fn singular(s: &str) -> String {
    if let Some(stem) = s.strip_suffix("ies") {
        return format!("{stem}y");
    }
    for suffix in ["sses", "xes", "ches", "shes"] {
        if s.ends_with(suffix) {
            return s[..s.len() - 2].to_string();
        }
    }
    if s.ends_with('s') && !s.ends_with("ss") && !s.ends_with("us") {
        return s[..s.len() - 1].to_string();
    }
    s.to_string()
}

/// A UUID (version 4 layout) distinct for every `i < 2^62`.
pub fn unique_uuid(i: u64, key: u64) -> u128 {
    let lo = 0x8000_0000_0000_0000u64 | Perm::new(1u64 << 62, key).apply(i & ((1 << 62) - 1));
    let hi = (mix(i ^ key) & !0xF000) | 0x4000;
    ((hi as u128) << 64) | lo as u128
}

/// Truncates an instant to the precision of the column type.
pub fn truncate_time(us: i64, precision: Option<u8>) -> i64 {
    let unit = match precision {
        None => US_PER_SEC,
        Some(p) if p < 6 => 10i64.pow(6 - p as u32),
        Some(_) => 1,
    };
    us - us.rem_euclid(unit)
}

/// Values distinct for every row index, by construction.
#[derive(Debug, Clone)]
pub enum Unique {
    Int { start: i128, step: i128 },
    Decimal { start: i128, step: i128, scale: u8 },
    Float { start: f64, step: f64 },
    Uuid { key: u64 },
    Text { kind: TextKind, key: u64, max_len: Option<u32> },
    /// FixedString(n): the index in base 36, left-padded.
    Fixed { n: u32 },
    Time { start: i64, step: i64, precision: Option<u8>, key: u64 },
    Date { start: i32, step: i32 },
    Ip4 { start: u32 },
    Ip6 { start: u128 },
}

impl Unique {
    /// The value of row `i`.
    pub fn value(&self, i: u64) -> Value {
        match self {
            Unique::Int { start, step } => Value::Int(start + step * i as i128),
            Unique::Decimal { start, step, scale } => Value::Decimal(start + step * i as i128, *scale),
            Unique::Float { start, step } => Value::Float(start + step * i as f64),
            Unique::Uuid { key } => Value::Uuid(unique_uuid(i, *key)),
            Unique::Text { kind, key, max_len } => {
                let mut rng = Rng::at(&[*key, i]);
                let s = kind.render_unique(&mut rng, i);
                // Keep the distinguishing suffix when the value must be shortened.
                Value::Str(match max_len {
                    Some(m) if s.chars().count() > *m as usize => {
                        let tail = format!("{}", i + 1);
                        let keep = (*m as usize).saturating_sub(tail.len());
                        s.chars().take(keep).collect::<String>() + &tail
                    }
                    _ => s,
                })
            }
            Unique::Fixed { n } => Value::Str(base36(i, *n as usize)),
            Unique::Time { start, step, precision, key } => {
                let jitter = (mix(i ^ key) % (*step).max(1) as u64) as i64;
                Value::Time(truncate_time(start + step * i as i64 + jitter, *precision).max(start + step * i as i64))
            }
            Unique::Date { start, step } => Value::Date(start + step * i as i32),
            Unique::Ip4 { start } => Value::Ip4(start.wrapping_add(i as u32)),
            Unique::Ip6 { start } => Value::Ip6(start.wrapping_add(i as u128)),
        }
    }
}

fn base36(mut i: u64, width: usize) -> String {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(i % 36) as usize];
        i /= 36;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// A domain of random values.
#[derive(Debug, Clone)]
pub enum Domain {
    Values { values: Vec<Value>, cumulative: Vec<u64> },
    Int { lo: i128, hi: i128 },
    Float { lo: f64, hi: f64 },
    Decimal { lo: i128, hi: i128, scale: u8 },
    Bool { p_true: f64 },
    Time { lo: i64, hi: i64, precision: Option<u8> },
    /// Spread over `[lo, hi)` in row order: row `i` of `rows` falls in the `i`-th slice.
    /// Used for partitioning columns, so each batch touches few partitions.
    Ordered { lo: i64, hi: i64, rows: u64, precision: Option<u8>, date: bool },
    Date { lo: i32, hi: i32 },
    Uuid,
    Text { kind: TextKind, max_len: Option<u32>, pool: Option<u32> },
    Fixed { n: u32 },
    Ip4,
    Ip6,
    Json,
    Array { min: u32, max: u32, elem: Box<Elem> },
    Map { min: u32, max: u32, key: Box<Elem>, value: Box<Elem> },
    Tuple(Vec<Elem>),
    /// For types dbgen cannot generate: always NULL.
    Null,
}

/// The domain of the elements of an array, map or tuple.
#[derive(Debug, Clone)]
pub struct Elem {
    pub domain: Domain,
    pub null_rate: f64,
}

impl Domain {
    /// A domain drawing from `values`, in proportion to `weights` (equal by default).
    pub fn values(values: Vec<Value>, weights: Option<&[u32]>) -> Domain {
        let mut cumulative = Vec::with_capacity(values.len());
        let mut total = 0u64;
        for k in 0..values.len() {
            total += weights.and_then(|w| w.get(k)).copied().unwrap_or(1) as u64;
            cumulative.push(total);
        }
        Domain::Values { values, cumulative }
    }

    /// A value for row `i`, drawn from `rng`.
    pub fn sample(&self, rng: &mut Rng, i: u64) -> Value {
        match self {
            Domain::Values { values, cumulative } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    values[rng.weighted_cumulative(cumulative)].clone()
                }
            }
            Domain::Int { lo, hi } => Value::Int(rng.between_i128(*lo, *hi)),
            Domain::Float { lo, hi } => Value::Float(lo + (hi - lo) * rng.f64()),
            Domain::Decimal { lo, hi, scale } => Value::Decimal(rng.between_i128(*lo, *hi), *scale),
            Domain::Bool { p_true } => Value::Bool(rng.chance(*p_true)),
            Domain::Time { lo, hi, precision } => Value::Time(truncate_time(rng.between_i128(*lo as i128, *hi as i128) as i64, *precision)),
            Domain::Ordered { lo, hi, rows, precision, date } => {
                let span = (hi - lo) as f64;
                let pos = (i as f64 + rng.f64()) / (*rows).max(1) as f64;
                let t = lo + (span * pos.min(0.999_999_999)) as i64;
                if *date {
                    Value::Date(day_of(t) as i32)
                } else {
                    Value::Time(truncate_time(t, *precision))
                }
            }
            Domain::Date { lo, hi } => Value::Date(rng.between_i128(*lo as i128, *hi as i128) as i32),
            Domain::Uuid => Value::Uuid(rng.uuid_v4()),
            Domain::Text { kind, max_len, pool } => {
                let mut s = kind.render(rng, *pool);
                if let Some(m) = max_len {
                    if s.chars().count() > *m as usize {
                        s = s.chars().take(*m as usize).collect();
                    }
                }
                Value::Str(s)
            }
            Domain::Fixed { n } => Value::Str((0..*n).map(|_| (b'A' + rng.below(26) as u8) as char).collect()),
            Domain::Ip4 => Value::Ip4(0x0A00_0000 | (rng.next_u64() as u32 & 0x00FF_FFFF)),
            Domain::Ip6 => Value::Ip6((0xfd00u128 << 112) | (((rng.next_u64() as u128) << 64 | rng.next_u64() as u128) & ((1u128 << 112) - 1))),
            Domain::Json => Value::Str(TextKind::Json.render(rng, None)),
            Domain::Array { min, max, elem } => {
                let n = *min as u64 + rng.below((max - min) as u64 + 1);
                Value::Array((0..n).map(|_| elem.sample(rng, i)).collect())
            }
            Domain::Map { min, max, key, value } => {
                let n = *min as u64 + rng.below((max - min) as u64 + 1);
                let mut entries: Vec<(Value, Value)> = Vec::new();
                for _ in 0..n {
                    let k = key.sample(rng, i);
                    if !entries.iter().any(|(e, _)| *e == k) {
                        entries.push((k, value.sample(rng, i)));
                    }
                }
                Value::Map(entries)
            }
            Domain::Tuple(items) => Value::Tuple(items.iter().map(|e| e.sample(rng, i)).collect()),
            Domain::Null => Value::Null,
        }
    }

    /// Restricts the domain by `op value`, within the type's limits. Returns false when the
    /// bound cannot be applied to this domain.
    pub fn narrow(&mut self, op: CmpOp, v: &Value, ty: &DataType) -> bool {
        if op == CmpOp::Ne {
            return false;
        }
        match self {
            Domain::Int { lo, hi } => {
                let Some(x) = v.num() else { return false };
                let (tmin, tmax) = ty.int_range().unwrap_or((i128::MIN, i128::MAX));
                let f = x.to_f64();
                let (new_lo, new_hi) = match op {
                    CmpOp::Gt => (Some(f.floor() as i128 + 1), None),
                    CmpOp::Ge => (Some(f.ceil() as i128), None),
                    CmpOp::Lt => (None, Some(f.ceil() as i128 - 1)),
                    CmpOp::Le => (None, Some(f.floor() as i128)),
                    CmpOp::Eq => (Some(f as i128), Some(f as i128)),
                    CmpOp::Ne => unreachable!(),
                };
                apply_bounds(lo, hi, new_lo, new_hi, tmin, tmax);
                true
            }
            Domain::Decimal { lo, hi, scale } => {
                let Some(x) = v.num().and_then(|n| n.to_scaled(*scale)) else { return false };
                let DataType::Decimal { precision, scale: s } = ty.base() else { return false };
                let max = 10i128.pow(*precision as u32).saturating_sub(1);
                let _ = s;
                let (new_lo, new_hi) = match op {
                    CmpOp::Gt => (Some(x + 1), None),
                    CmpOp::Ge => (Some(x), None),
                    CmpOp::Lt => (None, Some(x - 1)),
                    CmpOp::Le => (None, Some(x)),
                    CmpOp::Eq => (Some(x), Some(x)),
                    CmpOp::Ne => unreachable!(),
                };
                apply_bounds(lo, hi, new_lo, new_hi, -max, max);
                true
            }
            Domain::Float { lo, hi } => {
                let Some(x) = v.as_f64() else { return false };
                let eps = x.abs().max(1.0) * 1e-9;
                match op {
                    CmpOp::Gt => *lo = lo.max(x + eps),
                    CmpOp::Ge => *lo = lo.max(x),
                    CmpOp::Lt => *hi = hi.min(x - eps),
                    CmpOp::Le => *hi = hi.min(x),
                    CmpOp::Eq => {
                        *lo = x;
                        *hi = x;
                    }
                    CmpOp::Ne => unreachable!(),
                }
                if lo > hi {
                    let width = 1000.0f64.max(x.abs());
                    if matches!(op, CmpOp::Gt | CmpOp::Ge) {
                        *hi = *lo + width;
                    } else {
                        *lo = *hi - width;
                    }
                }
                true
            }
            Domain::Time { lo, hi, .. } | Domain::Ordered { lo, hi, .. } => {
                let Some(t) = v.as_time() else { return false };
                let (mut l, mut h) = (*lo as i128, *hi as i128);
                let (new_lo, new_hi) = match op {
                    CmpOp::Gt => (Some(t as i128 + 1), None),
                    CmpOp::Ge => (Some(t as i128), None),
                    CmpOp::Lt => (None, Some(t as i128 - 1)),
                    CmpOp::Le => (None, Some(t as i128)),
                    CmpOp::Eq => (Some(t as i128), Some(t as i128)),
                    CmpOp::Ne => unreachable!(),
                };
                apply_bounds(&mut l, &mut h, new_lo, new_hi, i64::MIN as i128 / 2, i64::MAX as i128 / 2);
                *lo = l as i64;
                *hi = h as i64;
                true
            }
            Domain::Date { lo, hi } => {
                let Some(t) = v.as_time() else { return false };
                let d = day_of(t) as i128;
                let (mut l, mut h) = (*lo as i128, *hi as i128);
                let (new_lo, new_hi) = match op {
                    CmpOp::Gt => (Some(d + 1), None),
                    CmpOp::Ge => (Some(d), None),
                    CmpOp::Lt => (None, Some(d - 1)),
                    CmpOp::Le => (None, Some(d)),
                    CmpOp::Eq => (Some(d), Some(d)),
                    CmpOp::Ne => unreachable!(),
                };
                apply_bounds(&mut l, &mut h, new_lo, new_hi, i32::MIN as i128, i32::MAX as i128);
                *lo = l as i32;
                *hi = h as i32;
                true
            }
            _ => false,
        }
    }
}

/// Intersects `[lo, hi]` with the new bounds; when the intersection is empty, keeps a
/// range of the old width on the bounded side, inside the type limits.
fn apply_bounds(lo: &mut i128, hi: &mut i128, new_lo: Option<i128>, new_hi: Option<i128>, tmin: i128, tmax: i128) {
    let width = hi.saturating_sub(*lo);
    if let Some(l) = new_lo {
        *lo = (*lo).max(l).clamp(tmin, tmax);
    }
    if let Some(h) = new_hi {
        *hi = (*hi).min(h).clamp(tmin, tmax);
    }
    if lo > hi {
        if new_lo.is_some() && new_hi.is_none() {
            *hi = lo.saturating_add(width).min(tmax);
        } else if new_hi.is_some() && new_lo.is_none() {
            *lo = hi.saturating_sub(width).max(tmin);
        }
    }
}

impl Elem {
    /// A value for row `i`, drawn from `rng`.
    pub fn sample(&self, rng: &mut Rng, i: u64) -> Value {
        if self.null_rate > 0.0 && rng.chance(self.null_rate) {
            return Value::Null;
        }
        self.domain.sample(rng, i)
    }
}

/// A plausible numeric range for a column, from its name. The last word usually says
/// what the number is (`heart_rate_count` is a count), so it decides first.
fn numeric_range(column: &str) -> (f64, f64) {
    let col = column.to_ascii_lowercase();
    let tokens: Vec<&str> = col.split(['_', '.']).filter(|t| !t.is_empty()).collect();
    let by_word = |w: &str| -> Option<(f64, f64)> {
        Some(match w {
            "count" | "qty" | "quantity" | "attempts" | "retries" | "level" | "rank" | "score" | "capacity" | "version" | "num" => (1.0, 100.0),
            "sum" | "total" | "amount" | "price" | "cost" | "balance" | "salary" | "revenue" => (0.0, 10_000.0),
            "sec" | "secs" | "seconds" | "duration" => (0.0, 3600.0),
            "ms" | "millis" | "milliseconds" => (0.0, 60_000.0),
            "min" | "mins" | "minutes" => (0.0, 600.0),
            "hours" => (0.0, 72.0),
            "days" => (0.0, 365.0),
            "percent" | "pct" | "percentage" => (0.0, 100.0),
            "ratio" | "probability" | "share" | "fraction" | "weight" => (0.0, 1.0),
            "age" => (0.0, 100.0),
            "year" => (1990.0, 2030.0),
            "month" => (1.0, 12.0),
            "day" => (1.0, 28.0),
            "hour" => (0.0, 23.0),
            "lat" | "latitude" => (-90.0, 90.0),
            "lon" | "lng" | "longitude" => (-180.0, 180.0),
            "bpm" => (40.0, 140.0),
            "celsius" | "temperature" => (35.0, 40.0),
            _ => return None,
        })
    };
    if let Some(r) = tokens.last().and_then(|w| by_word(w)) {
        return r;
    }
    tokens.iter().rev().find_map(|w| by_word(w)).unwrap_or((0.0, 10_000.0))
}

/// The default domain for a column of type `ty` named `column` in `table`; `window` is
/// the configured time range.
pub fn default_domain(ty: &DataType, table: &str, column: &str, window: (i64, i64)) -> Domain {
    let lc = ty.is_low_cardinality();
    let col = column.to_ascii_lowercase();
    match ty.base() {
        DataType::Bool => Domain::Bool { p_true: 0.5 },
        DataType::Int { .. } => {
            let (tmin, tmax) = ty.int_range().unwrap_or((0, 0));
            let (lo, hi) = numeric_range(column);
            Domain::Int { lo: (lo as i128).clamp(tmin, tmax), hi: (hi as i128).clamp(tmin, tmax) }
        }
        DataType::Float { .. } => {
            let (lo, hi) = numeric_range(column);
            Domain::Float { lo, hi }
        }
        DataType::Decimal { precision, scale } => {
            let (lo, hi) = numeric_range(column);
            let max = 10i128.checked_pow(*precision as u32).map_or(i128::MAX, |p| p - 1);
            let unit = 10f64.powi(*scale as i32);
            let to = |x: f64| ((x * unit).round() as i128).clamp(-max, max);
            Domain::Decimal { lo: to(lo), hi: to(hi), scale: *scale }
        }
        DataType::String { max_len } => Domain::Text { kind: TextKind::infer(table, column), max_len: *max_len, pool: lc.then_some(20) },
        DataType::FixedString(n) => Domain::Fixed { n: *n },
        DataType::Uuid => Domain::Uuid,
        DataType::Date | DataType::Date32 => {
            let date16 = matches!(ty.base(), DataType::Date);
            let (lo, hi) = if col.contains("birth") || col == "dob" {
                (days_from_civil(1930, 1, 1) as i32, days_from_civil(2005, 12, 31) as i32)
            } else {
                (day_of(window.0) as i32, (day_of(window.1 - 1)) as i32)
            };
            if date16 {
                Domain::Date { lo: lo.clamp(0, 65_535), hi: hi.clamp(0, 65_535) }
            } else {
                Domain::Date { lo, hi }
            }
        }
        DataType::DateTime { precision, .. } => {
            let (mut lo, mut hi) = (window.0, window.1 - 1);
            if precision.is_none() {
                // 32-bit seconds since the epoch.
                lo = lo.clamp(0, u32::MAX as i64 * US_PER_SEC);
                hi = hi.clamp(0, u32::MAX as i64 * US_PER_SEC);
            }
            if col.contains("birth") {
                lo = days_from_civil(1930, 1, 1) * US_PER_DAY;
                hi = days_from_civil(2005, 12, 31) * US_PER_DAY;
            }
            Domain::Time { lo, hi, precision: *precision }
        }
        DataType::Enum { variants, .. } => Domain::values(variants.iter().map(|(n, _)| Value::Str(n.clone())).collect(), None),
        DataType::Ipv4 => Domain::Ip4,
        DataType::Ipv6 => Domain::Ip6,
        DataType::Json => Domain::Json,
        DataType::Array(t) => Domain::Array { min: 0, max: 5, elem: Box::new(elem(t, table, column, window)) },
        DataType::Map(k, v) => Domain::Map {
            min: 0,
            max: 3,
            key: Box::new(elem(k, table, column, window)),
            value: Box::new(elem(v, table, column, window)),
        },
        DataType::Tuple(items) => Domain::Tuple(items.iter().map(|(n, t)| elem(t, table, n.as_deref().unwrap_or(column), window)).collect()),
        DataType::Nullable(_) | DataType::LowCardinality(_) => unreachable!("base() strips wrappers"),
        DataType::Unsupported(_) => Domain::Null,
    }
}

fn elem(t: &DataType, table: &str, column: &str, window: (i64, i64)) -> Elem {
    Elem { domain: default_domain(t, table, column, window), null_rate: if t.is_nullable() { 0.1 } else { 0.0 } }
}

/// How many distinct values a domain can produce (`None` = effectively unbounded).
pub fn domain_size(d: &Domain) -> Option<u128> {
    match d {
        Domain::Values { values, .. } => Some(values.len() as u128),
        Domain::Int { lo, hi } => Some((hi - lo) as u128 + 1),
        Domain::Bool { .. } => Some(2),
        Domain::Date { lo, hi } => Some((hi - lo) as u128 + 1),
        Domain::Null => Some(1),
        _ => None,
    }
}

/// The `k`-th distinct value of a finite domain (for enumerating composite keys).
pub fn domain_nth(d: &Domain, k: u64) -> Value {
    match d {
        Domain::Values { values, .. } => values[k as usize].clone(),
        Domain::Int { lo, .. } => Value::Int(lo + k as i128),
        Domain::Bool { .. } => Value::Bool(k == 1),
        Domain::Date { lo, .. } => Value::Date(lo + k as i32),
        _ => Value::Null,
    }
}

/// Converts a decimal or integer value to `scale` decimal places, when it fits.
pub fn scaled(v: &Value, scale: u8) -> Option<i128> {
    v.num().and_then(|n: Num| n.to_scaled(scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_heuristics() {
        assert_eq!(TextKind::infer("dim_staff", "notification_email"), TextKind::Email);
        assert_eq!(TextKind::infer("dim_patient", "first_name"), TextKind::FirstName);
        assert_eq!(TextKind::infer("dim_floor", "floor_name"), TextKind::Label("Floor".into()));
        assert_eq!(TextKind::infer("dim_alarm_types", "name"), TextKind::Label("Alarm type".into()));
        assert_eq!(TextKind::infer("fact_alarms", "alarm_status"), TextKind::Category("ALARM_STATUS".into()));
        assert_eq!(TextKind::infer("t", "trigger_details_json"), TextKind::Json);
        assert_eq!(TextKind::infer("t", "description"), TextKind::Sentence);
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("alarm_types"), "alarm_type");
        assert_eq!(singular("status"), "status");
        assert_eq!(table_stem("dim_room_types"), "room type");
    }

    #[test]
    fn unique_values_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..10_000 {
            assert!(seen.insert(unique_uuid(i, 9)));
        }
        let t = Unique::Time { start: 0, step: 1000, precision: Some(3), key: 1 };
        let mut prev = -1;
        for i in 0..1000 {
            let Value::Time(x) = t.value(i) else { panic!() };
            assert!(x > prev);
            assert_eq!(x % 1000, 0);
            prev = x;
        }
        let f = Unique::Fixed { n: 3 };
        assert_eq!(f.value(36), Value::Str("010".into()));
        let s = Unique::Text { kind: TextKind::Label("Floor".into()), key: 1, max_len: Some(6) };
        assert_eq!(s.value(122), Value::Str("Flo123".into()));
    }

    #[test]
    fn narrowing() {
        let ty = DataType::Int { bits: 32, signed: true };
        let mut d = Domain::Int { lo: 0, hi: 10_000 };
        assert!(d.narrow(CmpOp::Gt, &Value::Int(0), &ty));
        assert!(d.narrow(CmpOp::Le, &Value::Int(10), &ty));
        assert!(matches!(d, Domain::Int { lo: 1, hi: 10 }));
        let mut far = Domain::Int { lo: 0, hi: 100 };
        far.narrow(CmpOp::Ge, &Value::Int(1_000_000), &ty);
        assert!(matches!(far, Domain::Int { lo: 1_000_000, hi: 1_000_100 }));
        let dec = DataType::Decimal { precision: 4, scale: 3 };
        let mut w = Domain::Decimal { lo: 0, hi: 9_999, scale: 3 };
        w.narrow(CmpOp::Gt, &Value::Int(0), &dec);
        w.narrow(CmpOp::Le, &Value::Int(1), &dec);
        assert!(matches!(w, Domain::Decimal { lo: 1, hi: 1000, .. }));
    }
}
