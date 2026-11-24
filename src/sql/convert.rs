//! Conversion from the `sqlparser` AST to dbgen's model: data types, column definitions,
//! table constraints. The same SQL spelling can mean different things in different
//! dialects (`Int8` is 8 bits in ClickHouse and 64 in PostgreSQL; columns are nullable by
//! default in standard SQL and not in ClickHouse): [`TypeRules`] carries those choices.

use sqlparser::ast::{
    ArrayElemTypeDef, ColumnDef, ColumnOption, DataType as S, EnumMember, ExactNumberInfo, Expr, TableConstraint,
    TypeModifier, UnaryOperator, Value as SqlValue,
};

use super::{unqualified, CheckSpec, ColumnSpec, DefaultKind, DefaultSpec, ExprSpec, ForeignKeySpec, Statement};
use crate::schema::DataType;

#[derive(Debug, Clone, Copy)]
pub struct TypeRules {
    /// `Int8` is an 8-bit integer (ClickHouse) rather than PostgreSQL's 64-bit `int8`.
    pub int8_is_8_bits: bool,
    /// A column without `NOT NULL` accepts NULL (standard SQL), rather than only when its
    /// type says `Nullable(...)` (ClickHouse).
    pub nullable_by_default: bool,
    /// `VARCHAR(n)` limits length (standard SQL), rather than being an alias of an
    /// unbounded string (ClickHouse).
    pub varchar_has_length: bool,
    /// `DATE` is a 16-bit day count (ClickHouse) rather than a full-range date.
    pub date_is_16_bit: bool,
    /// `TIMESTAMP` / `DATETIME` without precision is 32-bit seconds (ClickHouse).
    pub timestamp_is_seconds: bool,
}

pub fn data_type(t: &S, rules: &TypeRules) -> DataType {
    let int = |bits: u16, signed: bool| DataType::Int { bits, signed };
    let string = |len: Option<u64>| DataType::String {
        max_len: if rules.varchar_has_length { len.map(|l| l.min(u32::MAX as u64) as u32) } else { None },
    };
    match t {
        S::Nullable(inner) => DataType::Nullable(Box::new(data_type(inner, rules))),
        S::LowCardinality(inner) => DataType::LowCardinality(Box::new(data_type(inner, rules))),
        S::Bool | S::Boolean => DataType::Bool,
        S::TinyInt(_) => int(8, true),
        S::Int8(_) if rules.int8_is_8_bits => int(8, true),
        S::Int8(_) => int(64, true),
        S::Int2(_) | S::SmallInt(_) | S::Int16 => int(16, true),
        S::Int(_) | S::Int4(_) | S::Integer(_) | S::MediumInt(_) | S::Int32 => int(32, true),
        S::BigInt(_) | S::Int64 | S::Signed | S::SignedInteger => int(64, true),
        S::Int128 | S::HugeInt => int(128, true),
        S::Int256 => int(256, true),
        S::TinyIntUnsigned(_) | S::UTinyInt | S::UInt8 => int(8, false),
        S::SmallIntUnsigned(_) | S::Int2Unsigned(_) | S::USmallInt | S::UInt16 => int(16, false),
        S::IntUnsigned(_) | S::Int4Unsigned(_) | S::IntegerUnsigned(_) | S::MediumIntUnsigned(_) | S::UInt32 => int(32, false),
        S::BigIntUnsigned(_) | S::Int8Unsigned(_) | S::UBigInt | S::UInt64 | S::Unsigned | S::UnsignedInteger => int(64, false),
        S::UInt128 | S::UHugeInt => int(128, false),
        S::UInt256 => int(256, false),
        S::Float4 | S::Float32 | S::Real | S::RealUnsigned => DataType::Float { bits: 32 },
        S::Float64 | S::Float8 | S::DoublePrecision | S::DoublePrecisionUnsigned | S::Double(_) | S::DoubleUnsigned(_) => {
            DataType::Float { bits: 64 }
        }
        S::Float(info) | S::FloatUnsigned(info) => match info {
            ExactNumberInfo::Precision(p) | ExactNumberInfo::PrecisionAndScale(p, _) if *p > 24 => DataType::Float { bits: 64 },
            _ => DataType::Float { bits: 32 },
        },
        S::Decimal(info) | S::DecimalUnsigned(info) | S::Numeric(info) | S::Dec(info) | S::DecUnsigned(info) | S::BigNumeric(info)
        | S::BigDecimal(info) => match info {
            ExactNumberInfo::None => DataType::Decimal { precision: 10, scale: 0 },
            ExactNumberInfo::Precision(p) => DataType::Decimal { precision: (*p).min(76) as u8, scale: 0 },
            ExactNumberInfo::PrecisionAndScale(p, s) => DataType::Decimal { precision: (*p).min(76) as u8, scale: (*s).clamp(0, 76) as u8 },
        },
        S::String(len) => string(*len),
        S::Text | S::TinyText | S::MediumText | S::LongText | S::Clob(_) | S::CharacterLargeObject(_) | S::CharLargeObject(_) => {
            DataType::String { max_len: None }
        }
        S::Varchar(len) | S::Nvarchar(len) | S::CharacterVarying(len) | S::CharVarying(len) | S::Character(len) | S::Char(len) => {
            string(len.as_ref().and_then(|l| match l {
                sqlparser::ast::CharacterLength::IntegerLength { length, .. } => Some(*length),
                sqlparser::ast::CharacterLength::Max => None,
            }))
        }
        S::Blob(_) | S::TinyBlob | S::MediumBlob | S::LongBlob | S::Bytea | S::Bytes(_) | S::Varbinary(_) => {
            DataType::String { max_len: None }
        }
        S::Binary(Some(n)) => DataType::FixedString(*n as u32),
        S::Binary(None) => DataType::String { max_len: None },
        S::FixedString(n) => DataType::FixedString(*n as u32),
        S::Uuid => DataType::Uuid,
        S::Date if rules.date_is_16_bit => DataType::Date,
        S::Date | S::Date32 => DataType::Date32,
        S::Datetime(None) | S::Timestamp(None, _) if rules.timestamp_is_seconds => DataType::DateTime { precision: None, tz: None },
        S::Datetime(p) | S::Timestamp(p, _) | S::TimestampNtz(p) => DataType::DateTime { precision: Some(p.unwrap_or(6).min(9) as u8), tz: None },
        S::Datetime64(p, tz) => DataType::DateTime { precision: Some((*p).min(9) as u8), tz: tz.clone() },
        S::JSON | S::JSONB => DataType::Json,
        S::Enum(members, bits) => enum_type(members, *bits),
        S::Array(elem) => match elem {
            ArrayElemTypeDef::AngleBracket(t)
            | ArrayElemTypeDef::SquareBracket(t, _)
            | ArrayElemTypeDef::Parenthesis(t)
            | ArrayElemTypeDef::ParenthesisNotNull(t)
            | ArrayElemTypeDef::Qualified(t, _) => DataType::Array(Box::new(data_type(t, rules))),
            ArrayElemTypeDef::None => DataType::Unsupported(t.to_string()),
        },
        S::Map(k, v, _) => DataType::Map(Box::new(data_type(k, rules)), Box::new(data_type(v, rules))),
        S::Tuple(fields) => DataType::Tuple(fields.iter().map(|f| (f.field_name.as_ref().map(|n| n.value.clone()), data_type(&f.field_type, rules))).collect()),
        S::Nested(cols) => DataType::Array(Box::new(DataType::Tuple(
            cols.iter().map(|c| (Some(c.name.value.clone()), data_type(&c.data_type, rules))).collect(),
        ))),
        S::Custom(name, mods) => custom_type(&unqualified(name), mods, rules).unwrap_or_else(|| DataType::Unsupported(t.to_string())),
        other => DataType::Unsupported(other.to_string()),
    }
}

fn enum_type(members: &[EnumMember], bits: Option<u8>) -> DataType {
    let mut variants = Vec::with_capacity(members.len());
    for (i, m) in members.iter().enumerate() {
        match m {
            EnumMember::Name(n) => variants.push((n.clone(), i as i16 + 1)),
            EnumMember::NamedValue(n, e) => variants.push((n.clone(), int_literal(e).unwrap_or(i as i64 + 1) as i16)),
        }
    }
    let fits_8 = variants.iter().all(|(_, v)| (-128..=127).contains(v));
    let bits = match bits {
        Some(16) => 16,
        Some(_) => 8,
        None if fits_8 => 8,
        None => 16,
    };
    DataType::Enum { bits, variants }
}

/// The integer value of a literal expression such as `3` or `-2`.
pub fn int_literal(e: &Expr) -> Option<i64> {
    match e {
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n.parse().ok(),
            _ => None,
        },
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => int_literal(expr).map(|v| -v),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } | Expr::Nested(expr) => int_literal(expr),
        _ => None,
    }
}

fn custom_type(name: &str, mods: &[TypeModifier], rules: &TypeRules) -> Option<DataType> {
    let number = |i: usize| match mods.get(i) {
        Some(TypeModifier::Number(n, _)) => n.parse::<u8>().ok(),
        _ => None,
    };
    let point = || DataType::Tuple(vec![(None, DataType::Float { bits: 64 }), (None, DataType::Float { bits: 64 })]);
    let ring = || DataType::Array(Box::new(point()));
    let polygon = || DataType::Array(Box::new(ring()));
    Some(match name {
        "IPv4" => DataType::Ipv4,
        "IPv6" => DataType::Ipv6,
        "Decimal32" => DataType::Decimal { precision: 9, scale: number(0)? },
        "Decimal64" => DataType::Decimal { precision: 18, scale: number(0)? },
        "Decimal128" => DataType::Decimal { precision: 38, scale: number(0)? },
        "Decimal256" => DataType::Decimal { precision: 76, scale: number(0)? },
        "Object" => DataType::Json,
        "Point" => point(),
        "Ring" | "LineString" => ring(),
        "Polygon" | "MultiLineString" => polygon(),
        "MultiPolygon" => DataType::Array(Box::new(polygon())),
        // SimpleAggregateFunction(f, T) stores plain T values.
        "SimpleAggregateFunction" => match mods.get(1) {
            Some(TypeModifier::Identifier(id)) => crate::dialect::parse_type_name(&id.value, rules)?,
            _ => return None,
        },
        _ => return None,
    })
}

/// An expression with its SQL text. `sqlparser`'s spans stop short of a function call's
/// closing parenthesis, so the text is the library's rendering rather than a source slice.
pub fn expr_spec(_stmt: &Statement, expr: Expr) -> ExprSpec {
    ExprSpec { sql: expr.to_string(), expr }
}

/// Converts one parsed column definition. `Nested(...)` columns expand to one array
/// column per field, as databases store them (`n.a Array(T)`).
pub fn column(stmt: &Statement, def: ColumnDef, rules: &TypeRules) -> Vec<ColumnSpec> {
    let name = def.name.value.clone();
    if let S::Nested(fields) = &def.data_type {
        return fields
            .iter()
            .map(|f| ColumnSpec {
                name: format!("{name}.{}", f.name.value),
                ty: DataType::Array(Box::new(data_type(&f.data_type, rules))),
                default: None,
                comment: None,
                primary_key: false,
                unique: false,
                references: None,
                check: None,
            })
            .collect();
    }
    let mut ty = data_type(&def.data_type, rules);
    let mut nullable: Option<bool> = None;
    let mut spec = ColumnSpec {
        name,
        ty: DataType::Bool,
        default: None,
        comment: None,
        primary_key: false,
        unique: false,
        references: None,
        check: None,
    };
    for opt in def.options {
        match opt.option {
            ColumnOption::Null => nullable = Some(true),
            ColumnOption::NotNull => nullable = Some(false),
            ColumnOption::Default(e) => spec.default = Some(DefaultSpec { kind: DefaultKind::Default, expr: Some(expr_spec(stmt, e)) }),
            ColumnOption::Materialized(e) => {
                spec.default = Some(DefaultSpec { kind: DefaultKind::Materialized, expr: Some(expr_spec(stmt, e)) })
            }
            ColumnOption::Alias(e) => spec.default = Some(DefaultSpec { kind: DefaultKind::Alias, expr: Some(expr_spec(stmt, e)) }),
            ColumnOption::Ephemeral(e) => {
                spec.default = Some(DefaultSpec { kind: DefaultKind::Ephemeral, expr: e.map(|e| expr_spec(stmt, e)) })
            }
            ColumnOption::Generated { generation_expr: Some(e), .. } => {
                spec.default = Some(DefaultSpec { kind: DefaultKind::Materialized, expr: Some(expr_spec(stmt, e)) })
            }
            ColumnOption::Generated { .. } | ColumnOption::Identity(_) => {
                spec.default = Some(DefaultSpec { kind: DefaultKind::Identity, expr: None })
            }
            ColumnOption::DialectSpecific(tokens) => {
                if tokens.iter().any(|t| t.to_string().eq_ignore_ascii_case("AUTO_INCREMENT") || t.to_string().eq_ignore_ascii_case("AUTOINCREMENT")) {
                    spec.default = Some(DefaultSpec { kind: DefaultKind::Identity, expr: None });
                }
            }
            ColumnOption::Comment(c) => spec.comment = Some(c),
            ColumnOption::PrimaryKey(_) => spec.primary_key = true,
            ColumnOption::Unique(_) => spec.unique = true,
            ColumnOption::ForeignKey(fk) => {
                spec.references = Some(ForeignKeySpec {
                    columns: vec![spec.name.clone()],
                    ref_table: unqualified(&fk.foreign_table),
                    ref_columns: fk.referred_columns.iter().map(|c| c.value.clone()).collect(),
                })
            }
            ColumnOption::Check(c) => {
                spec.check = Some(CheckSpec { name: c.name.map(|n| n.value), expr: expr_spec(stmt, *c.expr) });
            }
            _ => {}
        }
    }
    let is_nullable = ty.is_nullable();
    let want_nullable = match nullable {
        Some(n) => n,
        None => is_nullable || (rules.nullable_by_default && !spec.primary_key),
    };
    if want_nullable && !is_nullable {
        ty = wrap_nullable(ty);
    } else if !want_nullable && is_nullable {
        ty = unwrap_nullable(ty);
    }
    spec.ty = ty;
    vec![spec]
}

/// `Nullable` goes inside `LowCardinality`, as ClickHouse requires.
pub fn wrap_nullable(t: DataType) -> DataType {
    match t {
        DataType::LowCardinality(inner) => DataType::LowCardinality(Box::new(wrap_nullable(*inner))),
        DataType::Nullable(_) => t,
        other => DataType::Nullable(Box::new(other)),
    }
}

pub fn unwrap_nullable(t: DataType) -> DataType {
    match t {
        DataType::LowCardinality(inner) => DataType::LowCardinality(Box::new(unwrap_nullable(*inner))),
        DataType::Nullable(inner) => *inner,
        other => other,
    }
}

/// Converts a standard-SQL table constraint.
pub enum Constraint {
    Check(CheckSpec),
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    ForeignKey(ForeignKeySpec),
    Other,
}

pub fn table_constraint(stmt: &Statement, c: TableConstraint) -> Constraint {
    let cols = |cols: &[sqlparser::ast::IndexColumn]| -> Vec<String> {
        cols.iter()
            .map(|c| match &c.column.expr {
                Expr::Identifier(id) => id.value.clone(),
                other => other.to_string(),
            })
            .collect()
    };
    match c {
        TableConstraint::Check(c) => Constraint::Check(CheckSpec { name: c.name.map(|n| n.value), expr: expr_spec(stmt, *c.expr) }),
        TableConstraint::PrimaryKey(pk) => Constraint::PrimaryKey(cols(&pk.columns)),
        TableConstraint::Unique(u) => Constraint::Unique(cols(&u.columns)),
        TableConstraint::ForeignKey(fk) => Constraint::ForeignKey(ForeignKeySpec {
            columns: fk.columns.iter().map(|c| c.value.clone()).collect(),
            ref_table: unqualified(&fk.foreign_table),
            ref_columns: fk.referred_columns.iter().map(|c| c.value.clone()).collect(),
        }),
        _ => Constraint::Other,
    }
}
