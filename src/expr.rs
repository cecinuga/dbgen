//! SQL expressions over a generated row: compiled from the `sqlparser` AST into a form
//! with resolved column indices, evaluated with SQL three-valued logic, and analysed for
//! the structure the constraint solver exploits (bounds, literal domains, `col = expr`
//! assignments, orderings between columns).
//!
//! The evaluator covers the operators and the common functions CHECK constraints and
//! computed columns use. An expression using anything else does not compile; the plan
//! reports it, and such a constraint is left to the database.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use sqlparser::ast::{
    BinaryOperator as B, DateTimeField, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, InTarget, UnaryOperator as U,
    Value as SqlValue,
};

use crate::schema::DataType;
use crate::sql::convert::{data_type, TypeRules};
use crate::timeutil::{civil_from_days, day_of, days_from_civil, US_PER_DAY, US_PER_HOUR, US_PER_MIN, US_PER_SEC};
use crate::value::{date_of_time, parse_uuid, Num, Value};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn holds(self, o: Ordering) -> bool {
        match self {
            CmpOp::Eq => o == Ordering::Equal,
            CmpOp::Ne => o != Ordering::Equal,
            CmpOp::Lt => o == Ordering::Less,
            CmpOp::Le => o != Ordering::Greater,
            CmpOp::Gt => o == Ordering::Greater,
            CmpOp::Ge => o != Ordering::Less,
        }
    }

    /// The operator with its operands swapped (`a < b` <=> `b > a`).
    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
            other => other,
        }
    }
}

/// An arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    IntDiv,
}

/// The SQL functions the evaluator knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    IsNull,
    IsNotNull,
    Coalesce,
    NullIf,
    Identity,
    If,
    MultiIf,
    Xor,
    Abs,
    Greatest,
    Least,
    Round,
    Floor,
    Ceil,
    Bytes,
    Chars,
    Empty,
    NotEmpty,
    Lower,
    Upper,
    Trim,
    Concat,
    Substring,
    StartsWith,
    EndsWith,
    ToString,
    ToInt,
    ToFloat,
    ToDecimal,
    ToDate,
    ToDateTime,
    ToUuid,
    ToBool,
    Now,
    Today,
    Yyyymm,
    Yyyymmdd,
    Year,
    Month,
    DayOfMonth,
    Hour,
    StartOfDay,
    StartOfMonth,
    DateDiff,
    UnixSeconds,
    UnixMillis,
    UnixMicros,
    Has,
}

/// A compiled expression: columns are row indices, functions are resolved.
#[derive(Debug, Clone)]
pub enum CExpr {
    Lit(Value),
    Col(usize),
    Not(Box<CExpr>),
    Neg(Box<CExpr>),
    And(Box<CExpr>, Box<CExpr>),
    Or(Box<CExpr>, Box<CExpr>),
    Cmp(CmpOp, Box<CExpr>, Box<CExpr>),
    Arith(ArithOp, Box<CExpr>, Box<CExpr>),
    IsNull(Box<CExpr>, bool),
    In(Box<CExpr>, Vec<CExpr>, bool),
    Between(Box<CExpr>, Box<CExpr>, Box<CExpr>, bool),
    Like(Box<CExpr>, Box<CExpr>, bool, bool),
    Case { operand: Option<Box<CExpr>>, whens: Vec<(CExpr, CExpr)>, otherwise: Option<Box<CExpr>> },
    Func(Func, Vec<CExpr>),
    Cast(Box<CExpr>, DataType),
    Tuple(Vec<CExpr>),
    Array(Vec<CExpr>),
}

/// Resolves column names and converts types while compiling.
pub struct Compiler<'a> {
    pub resolve: &'a dyn Fn(&str) -> Option<usize>,
    pub rules: TypeRules,
}

fn b(e: CExpr) -> Box<CExpr> {
    Box::new(e)
}

impl Compiler<'_> {
    /// Compiles a parsed expression; fails on columns, operators or functions the
    /// evaluator does not know.
    pub fn compile(&self, e: &Expr) -> Result<CExpr, String> {
        Ok(match e {
            Expr::Identifier(id) => CExpr::Col((self.resolve)(&id.value).ok_or_else(|| format!("unknown column {}", id.value))?),
            Expr::CompoundIdentifier(parts) => {
                let joined = parts.iter().map(|p| p.value.as_str()).collect::<Vec<_>>().join(".");
                let last = parts.last().map(|p| p.value.as_str()).unwrap_or_default();
                CExpr::Col((self.resolve)(&joined).or_else(|| (self.resolve)(last)).ok_or_else(|| format!("unknown column {joined}"))?)
            }
            Expr::Value(v) => CExpr::Lit(literal(&v.value)?),
            Expr::Nested(inner) => self.compile(inner)?,
            Expr::UnaryOp { op: U::Not, expr } => CExpr::Not(b(self.compile(expr)?)),
            Expr::UnaryOp { op: U::Minus, expr } => match self.compile(expr)? {
                CExpr::Lit(v) => CExpr::Lit(v.num().and_then(Num::neg).map(Num::into_value).ok_or("cannot negate literal")?),
                other => CExpr::Neg(b(other)),
            },
            Expr::UnaryOp { op: U::Plus, expr } => self.compile(expr)?,
            Expr::BinaryOp { left, op, right } => {
                let l = self.compile(left)?;
                let r = self.compile(right)?;
                match op {
                    B::And => CExpr::And(b(l), b(r)),
                    B::Or => CExpr::Or(b(l), b(r)),
                    B::Xor => CExpr::Func(Func::Xor, vec![l, r]),
                    B::Eq => CExpr::Cmp(CmpOp::Eq, b(l), b(r)),
                    B::NotEq => CExpr::Cmp(CmpOp::Ne, b(l), b(r)),
                    B::Lt => CExpr::Cmp(CmpOp::Lt, b(l), b(r)),
                    B::LtEq => CExpr::Cmp(CmpOp::Le, b(l), b(r)),
                    B::Gt => CExpr::Cmp(CmpOp::Gt, b(l), b(r)),
                    B::GtEq => CExpr::Cmp(CmpOp::Ge, b(l), b(r)),
                    B::Plus => CExpr::Arith(ArithOp::Add, b(l), b(r)),
                    B::Minus => CExpr::Arith(ArithOp::Sub, b(l), b(r)),
                    B::Multiply => CExpr::Arith(ArithOp::Mul, b(l), b(r)),
                    B::Divide => CExpr::Arith(ArithOp::Div, b(l), b(r)),
                    B::Modulo => CExpr::Arith(ArithOp::Rem, b(l), b(r)),
                    B::MyIntegerDivide | B::DuckIntegerDivide => CExpr::Arith(ArithOp::IntDiv, b(l), b(r)),
                    B::StringConcat => CExpr::Func(Func::Concat, vec![l, r]),
                    other => return Err(format!("operator {other} is not supported")),
                }
            }
            Expr::IsNull(e) => CExpr::IsNull(b(self.compile(e)?), false),
            Expr::IsNotNull(e) => CExpr::IsNull(b(self.compile(e)?), true),
            Expr::IsTrue(e) => CExpr::Cmp(CmpOp::Eq, b(self.compile(e)?), b(CExpr::Lit(Value::Bool(true)))),
            Expr::IsFalse(e) => CExpr::Cmp(CmpOp::Eq, b(self.compile(e)?), b(CExpr::Lit(Value::Bool(false)))),
            Expr::In { expr, target: InTarget::List(items), negated, .. } => {
                CExpr::In(b(self.compile(expr)?), items.iter().map(|i| self.compile(i)).collect::<Result<_, _>>()?, *negated)
            }
            Expr::Between { expr, negated, low, high } => {
                CExpr::Between(b(self.compile(expr)?), b(self.compile(low)?), b(self.compile(high)?), *negated)
            }
            Expr::Like { negated, any: false, expr, pattern, escape_char: None } => {
                CExpr::Like(b(self.compile(expr)?), b(self.compile(pattern)?), *negated, false)
            }
            Expr::ILike { negated, any: false, expr, pattern, escape_char: None } => {
                CExpr::Like(b(self.compile(expr)?), b(self.compile(pattern)?), *negated, true)
            }
            Expr::Case { operand, conditions, else_result, .. } => CExpr::Case {
                operand: operand.as_ref().map(|o| self.compile(o)).transpose()?.map(b),
                whens: conditions.iter().map(|w| Ok((self.compile(&w.condition)?, self.compile(&w.result)?))).collect::<Result<_, String>>()?,
                otherwise: else_result.as_ref().map(|e| self.compile(e)).transpose()?.map(b),
            },
            Expr::Cast { expr, data_type: t, .. } => CExpr::Cast(b(self.compile(expr)?), data_type(t, &self.rules)),
            Expr::Tuple(items) => CExpr::Tuple(items.iter().map(|i| self.compile(i)).collect::<Result<_, _>>()?),
            Expr::Array(arr) => CExpr::Array(arr.elem.iter().map(|i| self.compile(i)).collect::<Result<_, _>>()?),
            Expr::Interval(iv) => {
                let n = match self.compile(&iv.value)? {
                    CExpr::Lit(v) => v.as_f64().or_else(|| match &v {
                        Value::Str(s) => s.split_whitespace().next().and_then(|n| n.parse().ok()),
                        _ => None,
                    }),
                    _ => None,
                }
                .ok_or("INTERVAL needs a constant")?;
                let unit = match &iv.leading_field {
                    Some(DateTimeField::Second | DateTimeField::Seconds) => US_PER_SEC,
                    Some(DateTimeField::Minute | DateTimeField::Minutes) => US_PER_MIN,
                    Some(DateTimeField::Hour | DateTimeField::Hours) => US_PER_HOUR,
                    Some(DateTimeField::Day | DateTimeField::Days) | None => US_PER_DAY,
                    Some(DateTimeField::Week(_) | DateTimeField::Weeks) => 7 * US_PER_DAY,
                    Some(DateTimeField::Month | DateTimeField::Months) => 30 * US_PER_DAY,
                    Some(DateTimeField::Year | DateTimeField::Years) => 365 * US_PER_DAY,
                    Some(other) => return Err(format!("INTERVAL unit {other} is not supported")),
                };
                CExpr::Lit(Value::Interval((n * unit as f64) as i64))
            }
            Expr::TypedString(ts) => {
                let lit = literal(&ts.value.value)?;
                CExpr::Cast(b(CExpr::Lit(lit)), data_type(&ts.data_type, &self.rules))
            }
            Expr::Function(f) => {
                if !matches!(f.parameters, FunctionArguments::None) {
                    return Err(format!("parametric function {} is not supported", f.name));
                }
                let args: Vec<CExpr> = match &f.args {
                    FunctionArguments::None => Vec::new(),
                    FunctionArguments::List(list) => list
                        .args
                        .iter()
                        .map(|a| match a {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => self.compile(e),
                            other => Err(format!("argument {other} is not supported")),
                        })
                        .collect::<Result<_, _>>()?,
                    FunctionArguments::Subquery(_) => return Err("subqueries are not supported".into()),
                };
                function(&f.name.to_string(), args)?
            }
            other => return Err(format!("expression {other} is not supported")),
        })
    }
}

fn literal(v: &SqlValue) -> Result<Value, String> {
    Ok(match v {
        SqlValue::Number(n, _) => parse_number(n).ok_or_else(|| format!("bad number {n}"))?,
        SqlValue::Boolean(x) => Value::Bool(*x),
        SqlValue::Null => Value::Null,
        SqlValue::SingleQuotedString(s)
        | SqlValue::EscapedStringLiteral(s)
        | SqlValue::NationalStringLiteral(s)
        | SqlValue::TripleSingleQuotedString(s)
        | SqlValue::TripleDoubleQuotedString(s) => Value::Str(s.clone()),
        SqlValue::DollarQuotedString(d) => Value::Str(d.value.clone()),
        other => return Err(format!("literal {other} is not supported")),
    })
}

/// `12` -> Int, `1.50` -> Decimal(150, 2), `1e3` -> Float.
pub fn parse_number(n: &str) -> Option<Value> {
    if let Some(hex) = n.strip_prefix("0x").or_else(|| n.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).ok().map(Value::Int);
    }
    if n.contains(['e', 'E']) {
        return n.parse::<f64>().ok().map(Value::Float);
    }
    match n.split_once('.') {
        None => n.parse::<i128>().ok().map(Value::Int),
        Some((int, frac)) => {
            let digits = format!("{int}{frac}");
            let scale = frac.len().min(38) as u8;
            digits.parse::<i128>().ok().map(|d| Value::Decimal(d, scale))
        }
    }
}

fn function(name: &str, mut args: Vec<CExpr>) -> Result<CExpr, String> {
    let lower = name.to_ascii_lowercase();
    let n = args.len();
    let bin = |op: CmpOp, mut a: Vec<CExpr>| -> Result<CExpr, String> {
        if a.len() != 2 {
            return Err(format!("{name} takes 2 arguments"));
        }
        let r = a.pop().unwrap();
        let l = a.pop().unwrap();
        Ok(CExpr::Cmp(op, b(l), b(r)))
    };
    let arith = |op: ArithOp, mut a: Vec<CExpr>| -> Result<CExpr, String> {
        if a.len() != 2 {
            return Err(format!("{name} takes 2 arguments"));
        }
        let r = a.pop().unwrap();
        let l = a.pop().unwrap();
        Ok(CExpr::Arith(op, b(l), b(r)))
    };
    let f = match lower.as_str() {
        "equals" => return bin(CmpOp::Eq, args),
        "notequals" => return bin(CmpOp::Ne, args),
        "less" => return bin(CmpOp::Lt, args),
        "lessorequals" => return bin(CmpOp::Le, args),
        "greater" => return bin(CmpOp::Gt, args),
        "greaterorequals" => return bin(CmpOp::Ge, args),
        "plus" => return arith(ArithOp::Add, args),
        "minus" => return arith(ArithOp::Sub, args),
        "multiply" => return arith(ArithOp::Mul, args),
        "divide" => return arith(ArithOp::Div, args),
        "modulo" => return arith(ArithOp::Rem, args),
        "intdiv" => return arith(ArithOp::IntDiv, args),
        "not" if n == 1 => return Ok(CExpr::Not(b(args.remove(0)))),
        "negate" if n == 1 => return Ok(CExpr::Neg(b(args.remove(0)))),
        "and" | "or" if n >= 2 => {
            let and = lower == "and";
            let mut it = args.into_iter();
            let first = it.next().unwrap();
            return Ok(it.fold(first, |acc, x| if and { CExpr::And(b(acc), b(x)) } else { CExpr::Or(b(acc), b(x)) }));
        }
        // ClickHouse stores `CASE x WHEN v THEN r ... ELSE e END` as
        // caseWithExpression(x, v, r, ..., e): compiled back to the CASE it was, so the
        // solver analyses it the same way.
        "casewithexpression" if n >= 4 && n % 2 == 0 => {
            let otherwise = args.pop().map(b);
            let mut rest = args.into_iter();
            let operand = rest.next().map(b);
            let mut whens = Vec::with_capacity((n - 2) / 2);
            while let (Some(value), Some(result)) = (rest.next(), rest.next()) {
                whens.push((value, result));
            }
            return Ok(CExpr::Case { operand, whens, otherwise });
        }
        "tuple" => return Ok(CExpr::Tuple(args)),
        "array" => return Ok(CExpr::Array(args)),
        "isnull" if n == 1 => Func::IsNull,
        "isnotnull" if n == 1 => Func::IsNotNull,
        "ifnull" | "coalesce" | "nvl" if n >= 1 => Func::Coalesce,
        "nullif" if n == 2 => Func::NullIf,
        "assumenotnull" | "tonullable" | "identity" if n == 1 => Func::Identity,
        "if" if n == 3 => Func::If,
        "multiif" if n >= 3 && n % 2 == 1 => Func::MultiIf,
        "xor" if n >= 2 => Func::Xor,
        "abs" if n == 1 => Func::Abs,
        "greatest" if n >= 1 => Func::Greatest,
        "least" if n >= 1 => Func::Least,
        "round" if (1..=2).contains(&n) => Func::Round,
        "floor" if n == 1 => Func::Floor,
        "ceil" | "ceiling" if n == 1 => Func::Ceil,
        "length" | "octet_length" if n == 1 => Func::Bytes,
        "lengthutf8" | "char_length" | "character_length" if n == 1 => Func::Chars,
        "empty" if n == 1 => Func::Empty,
        "notempty" if n == 1 => Func::NotEmpty,
        "lower" | "lowerutf8" | "lcase" if n == 1 => Func::Lower,
        "upper" | "upperutf8" | "ucase" if n == 1 => Func::Upper,
        "trim" | "trimboth" if n == 1 => Func::Trim,
        "concat" if n >= 1 => Func::Concat,
        "substring" | "substr" | "mid" if (2..=3).contains(&n) => Func::Substring,
        "startswith" if n == 2 => Func::StartsWith,
        "endswith" if n == 2 => Func::EndsWith,
        "tostring" if n == 1 => Func::ToString,
        l if l.starts_with("toint") || l.starts_with("touint") => Func::ToInt,
        l if l.starts_with("tofloat") => Func::ToFloat,
        l if l.starts_with("todecimal") && n == 2 => Func::ToDecimal,
        "todate" | "todate32" if n >= 1 => Func::ToDate,
        "todatetime" | "todatetime64" | "parsedatetimebesteffort" | "parsedatetime64besteffort" if n >= 1 => Func::ToDateTime,
        "touuid" if n == 1 => Func::ToUuid,
        "tobool" if n == 1 => Func::ToBool,
        "now" | "now64" | "current_timestamp" | "localtimestamp" => Func::Now,
        "today" | "current_date" => Func::Today,
        "toyyyymm" if n >= 1 => Func::Yyyymm,
        "toyyyymmdd" if n >= 1 => Func::Yyyymmdd,
        "toyear" | "year" if n >= 1 => Func::Year,
        "tomonth" | "month" if n >= 1 => Func::Month,
        "todayofmonth" | "day" | "dayofmonth" if n >= 1 => Func::DayOfMonth,
        "tohour" | "hour" if n >= 1 => Func::Hour,
        "tostartofday" if n >= 1 => Func::StartOfDay,
        "tostartofmonth" if n >= 1 => Func::StartOfMonth,
        "datediff" | "date_diff" | "timestampdiff" if n == 3 => Func::DateDiff,
        "tounixtimestamp" if n >= 1 => Func::UnixSeconds,
        "tounixtimestamp64milli" if n == 1 => Func::UnixMillis,
        "tounixtimestamp64micro" if n == 1 => Func::UnixMicros,
        "has" if n == 2 => Func::Has,
        _ => return Err(format!("function {name} with {n} argument(s) is not supported")),
    };
    Ok(CExpr::Func(f, args))
}

/// Evaluation context: the "current" instant that `now()` and `today()` return.
#[derive(Debug, Clone, Copy)]
pub struct EvalCtx {
    pub now_us: i64,
}

/// Evaluates `e` over `row` with SQL semantics: NULL propagates, and logic is
/// three-valued.
pub fn eval(e: &CExpr, row: &[Value], ctx: &EvalCtx) -> Value {
    match e {
        CExpr::Lit(v) => v.clone(),
        CExpr::Col(i) => row.get(*i).cloned().unwrap_or(Value::Null),
        CExpr::Not(x) => match eval(x, row, ctx).truth() {
            Some(t) => Value::Bool(!t),
            None => Value::Null,
        },
        CExpr::Neg(x) => eval(x, row, ctx).num().and_then(Num::neg).map_or(Value::Null, Num::into_value),
        CExpr::And(l, r) => {
            let a = eval(l, row, ctx).truth();
            if a == Some(false) {
                return Value::Bool(false);
            }
            match (a, eval(r, row, ctx).truth()) {
                (_, Some(false)) => Value::Bool(false),
                (Some(true), Some(true)) => Value::Bool(true),
                _ => Value::Null,
            }
        }
        CExpr::Or(l, r) => {
            let a = eval(l, row, ctx).truth();
            if a == Some(true) {
                return Value::Bool(true);
            }
            match (a, eval(r, row, ctx).truth()) {
                (_, Some(true)) => Value::Bool(true),
                (Some(false), Some(false)) => Value::Bool(false),
                _ => Value::Null,
            }
        }
        CExpr::Cmp(op, l, r) => match eval(l, row, ctx).compare(&eval(r, row, ctx)) {
            Some(o) => Value::Bool(op.holds(o)),
            None => Value::Null,
        },
        CExpr::Arith(op, l, r) => arith(*op, eval(l, row, ctx), eval(r, row, ctx)),
        CExpr::IsNull(x, negated) => Value::Bool(eval(x, row, ctx).is_null() != *negated),
        CExpr::In(x, items, negated) => {
            let v = eval(x, row, ctx);
            if v.is_null() {
                return Value::Null;
            }
            let mut saw_null = false;
            for item in items {
                match v.compare(&eval(item, row, ctx)) {
                    Some(Ordering::Equal) => return Value::Bool(!negated),
                    None => saw_null = true,
                    _ => {}
                }
            }
            if saw_null {
                Value::Null
            } else {
                Value::Bool(*negated)
            }
        }
        CExpr::Between(x, lo, hi, negated) => {
            let v = eval(x, row, ctx);
            match (v.compare(&eval(lo, row, ctx)), v.compare(&eval(hi, row, ctx))) {
                (Some(a), Some(c)) => Value::Bool((a != Ordering::Less && c != Ordering::Greater) != *negated),
                _ => Value::Null,
            }
        }
        CExpr::Like(x, p, negated, ci) => match (eval(x, row, ctx), eval(p, row, ctx)) {
            (Value::Str(s), Value::Str(pat)) => {
                let m = if *ci { like(&s.to_lowercase(), &pat.to_lowercase()) } else { like(&s, &pat) };
                Value::Bool(m != *negated)
            }
            _ => Value::Null,
        },
        CExpr::Case { operand, whens, otherwise } => {
            let op = operand.as_ref().map(|o| eval(o, row, ctx));
            for (w, r) in whens {
                let hit = match &op {
                    Some(v) => v.compare(&eval(w, row, ctx)) == Some(Ordering::Equal),
                    None => eval(w, row, ctx).truth() == Some(true),
                };
                if hit {
                    return eval(r, row, ctx);
                }
            }
            otherwise.as_ref().map_or(Value::Null, |o| eval(o, row, ctx))
        }
        CExpr::Func(f, args) => call(*f, args, row, ctx),
        CExpr::Cast(x, t) => cast(eval(x, row, ctx), t),
        CExpr::Tuple(items) => Value::Tuple(items.iter().map(|i| eval(i, row, ctx)).collect()),
        CExpr::Array(items) => Value::Array(items.iter().map(|i| eval(i, row, ctx)).collect()),
    }
}

/// A CHECK holds only when it evaluates to true: ClickHouse rejects a NULL result too.
pub fn holds(e: &CExpr, row: &[Value], ctx: &EvalCtx) -> bool {
    eval(e, row, ctx).truth() == Some(true)
}

fn arith(op: ArithOp, a: Value, c: Value) -> Value {
    use Value::*;
    match (&a, &c) {
        (Null, _) | (_, Null) => Null,
        (Time(t), Interval(d)) => match op {
            ArithOp::Add => Time(t + d),
            ArithOp::Sub => Time(t - d),
            _ => Null,
        },
        (Interval(d), Time(t)) if op == ArithOp::Add => Time(t + d),
        (Date(dd), Interval(d)) => match op {
            ArithOp::Add => Time(*dd as i64 * US_PER_DAY + d),
            ArithOp::Sub => Time(*dd as i64 * US_PER_DAY - d),
            _ => Null,
        },
        (Time(t), _) if c.num().is_some() => {
            let secs = (c.as_f64().unwrap_or(0.0) * US_PER_SEC as f64) as i64;
            match op {
                ArithOp::Add => Time(t + secs),
                ArithOp::Sub => Time(t - secs),
                _ => Null,
            }
        }
        (Date(d), _) if c.num().is_some() => {
            let days = c.as_f64().unwrap_or(0.0) as i32;
            match op {
                ArithOp::Add => Date(d + days),
                ArithOp::Sub => Date(d - days),
                _ => Null,
            }
        }
        (Time(x), Time(y)) if op == ArithOp::Sub => Int(((x - y) / US_PER_SEC) as i128),
        (Date(x), Date(y)) if op == ArithOp::Sub => Int((x - y) as i128),
        _ => {
            let (Some(x), Some(y)) = (a.num(), c.num()) else { return Null };
            let r = match op {
                ArithOp::Add => Num::add(x, y),
                ArithOp::Sub => Num::sub(x, y),
                ArithOp::Mul => Num::mul(x, y),
                ArithOp::Div => Num::div(x, y),
                ArithOp::Rem => Num::rem(x, y),
                ArithOp::IntDiv => Num::div(x, y).map(|q| Num::I(q.to_f64().trunc() as i128)),
            };
            r.map_or(Null, Num::into_value)
        }
    }
}

fn like(s: &str, pat: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    // Iterative matching with backtracking on the last `%`.
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if j < p.len() && p[j] == '\\' && j + 1 < p.len() && p[j + 1] == s[i] {
            i += 1;
            j += 2;
        } else if j < p.len() && (p[j] == '_' || p[j] == s[i]) && p[j] != '%' {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(st) = star {
            j = st + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Null => String::new(),
        other => {
            let s = other.to_string();
            s.trim_matches('\'').to_string()
        }
    }
}

fn call(f: Func, args: &[CExpr], row: &[Value], ctx: &EvalCtx) -> Value {
    let arg = |i: usize| eval(&args[i], row, ctx);
    let all = || args.iter().map(|a| eval(a, row, ctx)).collect::<Vec<_>>();
    match f {
        Func::IsNull => Value::Bool(arg(0).is_null()),
        Func::IsNotNull => Value::Bool(!arg(0).is_null()),
        Func::Coalesce => args.iter().map(|a| eval(a, row, ctx)).find(|v| !v.is_null()).unwrap_or(Value::Null),
        Func::NullIf => {
            let a = arg(0);
            if a.compare(&arg(1)) == Some(Ordering::Equal) {
                Value::Null
            } else {
                a
            }
        }
        Func::Identity => arg(0),
        Func::If => {
            if arg(0).truth() == Some(true) {
                arg(1)
            } else {
                arg(2)
            }
        }
        Func::MultiIf => {
            let mut i = 0;
            while i + 1 < args.len() {
                if arg(i).truth() == Some(true) {
                    return arg(i + 1);
                }
                i += 2;
            }
            arg(args.len() - 1)
        }
        Func::Xor => {
            let mut acc = false;
            for v in all() {
                match v.truth() {
                    Some(t) => acc ^= t,
                    None => return Value::Null,
                }
            }
            Value::Bool(acc)
        }
        Func::Abs => match arg(0).num() {
            Some(n) if n.to_f64() < 0.0 => n.neg().map_or(Value::Null, Num::into_value),
            Some(n) => n.into_value(),
            None => Value::Null,
        },
        Func::Greatest | Func::Least => {
            let vals = all();
            if vals.iter().any(Value::is_null) {
                return Value::Null;
            }
            let want = if f == Func::Greatest { Ordering::Greater } else { Ordering::Less };
            vals.into_iter().reduce(|a, c| if c.compare(&a) == Some(want) { c } else { a }).unwrap_or(Value::Null)
        }
        Func::Round | Func::Floor | Func::Ceil => {
            let Some(x) = arg(0).as_f64() else { return Value::Null };
            let digits = if args.len() > 1 { arg(1).as_f64().unwrap_or(0.0) as i32 } else { 0 };
            let p = 10f64.powi(digits);
            let r = match f {
                Func::Round => (x * p).round() / p,
                Func::Floor => (x * p).floor() / p,
                _ => (x * p).ceil() / p,
            };
            Value::Float(r)
        }
        Func::Bytes => match arg(0) {
            Value::Str(s) => Value::Int(s.len() as i128),
            Value::Array(a) => Value::Int(a.len() as i128),
            Value::Map(m) => Value::Int(m.len() as i128),
            Value::Null => Value::Null,
            other => Value::Int(text_of(&other).len() as i128),
        },
        Func::Chars => match arg(0) {
            Value::Str(s) => Value::Int(s.chars().count() as i128),
            Value::Null => Value::Null,
            other => Value::Int(text_of(&other).chars().count() as i128),
        },
        Func::Empty | Func::NotEmpty => {
            let empty = match arg(0) {
                Value::Str(s) => s.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Map(m) => m.is_empty(),
                Value::Uuid(u) => u == 0,
                Value::Null => return Value::Null,
                _ => false,
            };
            Value::Bool(empty == (f == Func::Empty))
        }
        Func::Lower => str_fn(arg(0), |s| s.to_lowercase()),
        Func::Upper => str_fn(arg(0), |s| s.to_uppercase()),
        Func::Trim => str_fn(arg(0), |s| s.trim().to_string()),
        Func::Concat => {
            let vals = all();
            if vals.iter().any(Value::is_null) {
                return Value::Null;
            }
            Value::Str(vals.iter().map(text_of).collect())
        }
        Func::Substring => {
            let Value::Str(s) = arg(0) else { return Value::Null };
            let chars: Vec<char> = s.chars().collect();
            let off = arg(1).as_f64().unwrap_or(1.0) as i64;
            let start = if off > 0 { (off - 1) as usize } else { chars.len().saturating_sub(off.unsigned_abs() as usize) };
            let len = if args.len() > 2 { arg(2).as_f64().unwrap_or(0.0).max(0.0) as usize } else { usize::MAX };
            Value::Str(chars.iter().skip(start).take(len).collect())
        }
        Func::StartsWith | Func::EndsWith => match (arg(0), arg(1)) {
            (Value::Str(s), Value::Str(p)) => Value::Bool(if f == Func::StartsWith { s.starts_with(&p) } else { s.ends_with(&p) }),
            _ => Value::Null,
        },
        Func::ToString => match arg(0) {
            Value::Null => Value::Null,
            v => Value::Str(text_of(&v)),
        },
        Func::ToInt => to_int(arg(0)),
        Func::ToFloat => match arg(0) {
            Value::Str(s) => s.trim().parse().map_or(Value::Null, Value::Float),
            v => v.as_f64().map_or(Value::Null, Value::Float),
        },
        Func::ToDecimal => {
            let scale = arg(1).as_f64().unwrap_or(0.0) as u8;
            cast(arg(0), &DataType::Decimal { precision: 38, scale })
        }
        Func::ToDate => cast(arg(0), &DataType::Date32),
        Func::ToDateTime => cast(arg(0), &DataType::DateTime { precision: Some(6), tz: None }),
        Func::ToUuid => cast(arg(0), &DataType::Uuid),
        Func::ToBool => cast(arg(0), &DataType::Bool),
        Func::Now => Value::Time(ctx.now_us),
        Func::Today => Value::Date(date_of_time(ctx.now_us)),
        Func::Yyyymm | Func::Yyyymmdd | Func::Year | Func::Month | Func::DayOfMonth => {
            let Some(t) = arg(0).as_time() else { return Value::Null };
            let (y, m, d) = civil_from_days(day_of(t));
            Value::Int(match f {
                Func::Yyyymm => (y * 100 + m as i64) as i128,
                Func::Yyyymmdd => (y * 10_000 + m as i64 * 100 + d as i64) as i128,
                Func::Year => y as i128,
                Func::Month => m as i128,
                _ => d as i128,
            })
        }
        Func::Hour => arg(0).as_time().map_or(Value::Null, |t| Value::Int((t.rem_euclid(US_PER_DAY) / US_PER_HOUR) as i128)),
        Func::StartOfDay => arg(0).as_time().map_or(Value::Null, |t| Value::Time(day_of(t) * US_PER_DAY)),
        Func::StartOfMonth => arg(0).as_time().map_or(Value::Null, |t| {
            let (y, m, _) = civil_from_days(day_of(t));
            Value::Date(days_from_civil(y, m, 1) as i32)
        }),
        Func::DateDiff => {
            let unit = text_of(&arg(0)).to_lowercase();
            let (Some(a), Some(c)) = (arg(1).as_time(), arg(2).as_time()) else { return Value::Null };
            let per = match unit.trim_end_matches('s') {
                "second" | "ss" | "s" => US_PER_SEC,
                "minute" | "mi" | "n" => US_PER_MIN,
                "hour" | "hh" | "h" => US_PER_HOUR,
                "day" | "dd" | "d" => US_PER_DAY,
                "week" | "wk" | "ww" => 7 * US_PER_DAY,
                "month" | "mm" | "m" => 30 * US_PER_DAY,
                "year" | "yyyy" | "yy" => 365 * US_PER_DAY,
                _ => return Value::Null,
            };
            Value::Int(((c - a) / per) as i128)
        }
        Func::UnixSeconds => arg(0).as_time().map_or(Value::Null, |t| Value::Int((t.div_euclid(US_PER_SEC)) as i128)),
        Func::UnixMillis => arg(0).as_time().map_or(Value::Null, |t| Value::Int((t / 1000) as i128)),
        Func::UnixMicros => arg(0).as_time().map_or(Value::Null, |t| Value::Int(t as i128)),
        Func::Has => match arg(0) {
            Value::Array(items) => {
                let x = arg(1);
                Value::Bool(items.iter().any(|i| i.compare(&x) == Some(Ordering::Equal)))
            }
            _ => Value::Null,
        },
    }
}

fn str_fn(v: Value, f: impl Fn(&str) -> String) -> Value {
    match v {
        Value::Str(s) => Value::Str(f(&s)),
        Value::Null => Value::Null,
        other => Value::Str(f(&text_of(&other))),
    }
}

fn to_int(v: Value) -> Value {
    match v {
        Value::Str(s) => s.trim().parse::<i128>().map_or(Value::Null, Value::Int),
        Value::Time(t) => Value::Int((t / US_PER_SEC) as i128),
        Value::Date(d) => Value::Int(d as i128),
        other => other.as_f64().map_or(Value::Null, |f| Value::Int(f.trunc() as i128)),
    }
}

/// Converts a value to a column type: used for `CAST`, derived assignments and
/// configured literals.
pub fn cast(v: Value, t: &DataType) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match t.base() {
        DataType::Bool => match &v {
            Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Value::Bool(true),
                "false" | "0" | "no" => Value::Bool(false),
                _ => Value::Null,
            },
            other => other.truth().map_or(Value::Null, Value::Bool),
        },
        DataType::Int { .. } => to_int(v),
        DataType::Float { .. } => match &v {
            Value::Str(s) => s.trim().parse().map_or(Value::Null, Value::Float),
            other => other.as_f64().map_or(Value::Null, Value::Float),
        },
        DataType::Decimal { scale, .. } => {
            let n = match &v {
                Value::Str(s) => parse_number(s.trim()).and_then(|x| x.num()),
                other => other.num(),
            };
            n.and_then(|n| n.to_scaled(*scale)).map_or(Value::Null, |d| Value::Decimal(d, *scale))
        }
        DataType::String { .. } | DataType::FixedString(_) | DataType::Enum { .. } | DataType::Json => Value::Str(text_of(&v)),
        DataType::Uuid => match &v {
            Value::Uuid(_) => v,
            Value::Str(s) => parse_uuid(s).map_or(Value::Null, Value::Uuid),
            _ => Value::Null,
        },
        DataType::Date | DataType::Date32 => v.as_time().map_or(Value::Null, |t| Value::Date(date_of_time(t))),
        DataType::DateTime { .. } => v.as_time().map_or(Value::Null, Value::Time),
        _ => v,
    }
}

// ---------------------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------------------

impl CExpr {
    /// Adds the columns the expression reads to `out`.
    pub fn columns(&self, out: &mut BTreeSet<usize>) {
        self.walk(&mut |e| {
            if let CExpr::Col(i) = e {
                out.insert(*i);
            }
        });
    }

    /// Whether the expression reads column `col`.
    pub fn references(&self, col: usize) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, CExpr::Col(i) if *i == col));
        found
    }

    fn walk(&self, f: &mut dyn FnMut(&CExpr)) {
        f(self);
        match self {
            CExpr::Lit(_) | CExpr::Col(_) => {}
            CExpr::Not(x) | CExpr::Neg(x) | CExpr::IsNull(x, _) | CExpr::Cast(x, _) => x.walk(f),
            CExpr::And(l, r) | CExpr::Or(l, r) | CExpr::Cmp(_, l, r) | CExpr::Arith(_, l, r) | CExpr::Like(l, r, _, _) => {
                l.walk(f);
                r.walk(f);
            }
            CExpr::In(x, items, _) => {
                x.walk(f);
                items.iter().for_each(|i| i.walk(f));
            }
            CExpr::Between(x, lo, hi, _) => {
                x.walk(f);
                lo.walk(f);
                hi.walk(f);
            }
            CExpr::Case { operand, whens, otherwise } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, r) in whens {
                    w.walk(f);
                    r.walk(f);
                }
                if let Some(o) = otherwise {
                    o.walk(f);
                }
            }
            CExpr::Func(_, args) | CExpr::Tuple(args) | CExpr::Array(args) => args.iter().for_each(|a| a.walk(f)),
        }
    }

    /// Top-level `AND` operands.
    pub fn conjuncts(&self) -> Vec<&CExpr> {
        match self {
            CExpr::And(l, r) => {
                let mut v = l.conjuncts();
                v.extend(r.conjuncts());
                v
            }
            other => vec![other],
        }
    }

    /// Columns whose NULL-ness the expression tests.
    pub fn null_tested(&self, out: &mut BTreeSet<usize>) {
        self.walk(&mut |e| match e {
            CExpr::IsNull(x, _) => x.columns(out),
            CExpr::Func(Func::IsNull | Func::IsNotNull | Func::Coalesce, args) => args.iter().for_each(|a| a.columns(out)),
            _ => {}
        });
    }

    /// `(column, literal)` pairs the expression compares: `c = 'x'`, `c IN (...)`,
    /// `CASE c WHEN 'x' ...`, `CASE WHEN c = 'x' ...`.
    pub fn literal_hints(&self, out: &mut Vec<(usize, Value)>) {
        self.walk(&mut |e| match e {
            CExpr::Cmp(CmpOp::Eq | CmpOp::Ne, l, r) => match (&**l, &**r) {
                (CExpr::Col(c), CExpr::Lit(v)) | (CExpr::Lit(v), CExpr::Col(c)) if !v.is_null() => out.push((*c, v.clone())),
                _ => {}
            },
            CExpr::In(x, items, _) => {
                if let CExpr::Col(c) = &**x {
                    for i in items {
                        if let CExpr::Lit(v) = i {
                            if !v.is_null() {
                                out.push((*c, v.clone()));
                            }
                        }
                    }
                }
            }
            CExpr::Case { operand: Some(op), whens, .. } => {
                if let CExpr::Col(c) = &**op {
                    for (w, _) in whens {
                        if let CExpr::Lit(v) = w {
                            out.push((*c, v.clone()));
                        }
                    }
                }
            }
            _ => {}
        });
    }

    /// `c op literal` (or `literal op c`, or `c BETWEEN a AND b`) as bounds on `c`.
    pub fn bounds(&self) -> Vec<(usize, CmpOp, Value)> {
        match self {
            CExpr::Cmp(op, l, r) => match (&**l, &**r) {
                (CExpr::Col(c), CExpr::Lit(v)) if !v.is_null() => vec![(*c, *op, v.clone())],
                (CExpr::Lit(v), CExpr::Col(c)) if !v.is_null() => vec![(*c, op.flip(), v.clone())],
                _ => vec![],
            },
            CExpr::Between(x, lo, hi, false) => match (&**x, &**lo, &**hi) {
                (CExpr::Col(c), CExpr::Lit(a), CExpr::Lit(z)) => vec![(*c, CmpOp::Ge, a.clone()), (*c, CmpOp::Le, z.clone())],
                _ => vec![],
            },
            _ => vec![],
        }
    }

    /// `c IN (literals)` (not negated): the closed set of values `c` may take.
    pub fn closed_domain(&self) -> Option<(usize, Vec<Value>)> {
        match self {
            CExpr::In(x, items, false) => {
                let CExpr::Col(c) = &**x else { return None };
                let vals: Option<Vec<Value>> = items.iter().map(|i| match i {
                    CExpr::Lit(v) if !v.is_null() => Some(v.clone()),
                    _ => None,
                }).collect();
                Some((*c, vals?))
            }
            CExpr::Cmp(CmpOp::Eq, l, r) => match (&**l, &**r) {
                (CExpr::Col(c), CExpr::Lit(v)) | (CExpr::Lit(v), CExpr::Col(c)) if !v.is_null() => Some((*c, vec![v.clone()])),
                _ => None,
            },
            _ => None,
        }
    }

    /// Ways to satisfy an equality by computing one column from the others:
    /// `c = expr` gives `c := expr`, and `a + b - c = k` gives `c := a + b - k` (or the
    /// equivalent for `a` and `b`).
    pub fn assignments(&self) -> Vec<(usize, CExpr)> {
        let CExpr::Cmp(CmpOp::Eq, l, r) = self else { return vec![] };
        let mut out = Vec::new();
        for (side, other) in [(&**l, &**r), (&**r, &**l)] {
            if let CExpr::Col(c) = side {
                if !other.references(*c) {
                    out.push((*c, other.clone()));
                }
            }
            // Linear isolation over a sum of signed terms.
            let mut terms = Vec::new();
            if flatten_sum(side, true, &mut terms) && terms.len() >= 2 {
                for (k, (term, positive)) in terms.iter().enumerate() {
                    let CExpr::Col(c) = term else { continue };
                    if other.references(*c) || terms.iter().enumerate().any(|(j, (t, _))| j != k && t.references(*c)) {
                        continue;
                    }
                    // c = other - rest (if +c) or rest - other (if -c).
                    let rest = terms
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != k)
                        .fold(CExpr::Lit(Value::Int(0)), |acc, (_, (t, pos))| {
                            CExpr::Arith(if *pos { ArithOp::Add } else { ArithOp::Sub }, b(acc), b(t.clone()))
                        });
                    let rhs = if *positive {
                        CExpr::Arith(ArithOp::Sub, b(other.clone()), b(rest))
                    } else {
                        CExpr::Arith(ArithOp::Sub, b(rest), b(other.clone()))
                    };
                    out.push((*c, rhs));
                }
            }
        }
        out
    }

    /// `a > b` / `a >= b` between two columns (possibly behind `a IS NULL OR ...`):
    /// `(later, earlier, strict)`.
    pub fn ordering(&self) -> Option<(usize, usize, bool)> {
        let cmp = |e: &CExpr| match e {
            CExpr::Cmp(op @ (CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le), l, r) => match (&**l, &**r) {
                (CExpr::Col(a), CExpr::Col(c)) => {
                    let strict = matches!(op, CmpOp::Gt | CmpOp::Lt);
                    if matches!(op, CmpOp::Gt | CmpOp::Ge) {
                        Some((*a, *c, strict))
                    } else {
                        Some((*c, *a, strict))
                    }
                }
                _ => None,
            },
            _ => None,
        };
        match self {
            CExpr::Or(l, r) => {
                for (x, y) in [(&**l, &**r), (&**r, &**l)] {
                    if let CExpr::IsNull(n, false) = x {
                        if let (CExpr::Col(nc), Some(o)) = (&**n, cmp(y)) {
                            if *nc == o.0 || *nc == o.1 {
                                return Some(o);
                            }
                        }
                    }
                }
                None
            }
            other => cmp(other),
        }
    }
}

fn flatten_sum(e: &CExpr, positive: bool, out: &mut Vec<(CExpr, bool)>) -> bool {
    match e {
        CExpr::Arith(ArithOp::Add, l, r) => flatten_sum(l, positive, out) && flatten_sum(r, positive, out),
        CExpr::Arith(ArithOp::Sub, l, r) => flatten_sum(l, positive, out) && flatten_sum(r, !positive, out),
        CExpr::Col(_) | CExpr::Lit(_) => {
            out.push((e.clone(), positive));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::clickhouse::RULES;

    const COLS: &[&str] = &["a", "b", "c", "status", "end_date", "start_date", "is_current"];

    fn compile(sql: &str) -> CExpr {
        let mut p = sqlparser::parser::Parser::new(&crate::dialect::clickhouse::DIALECT).try_with_sql(sql).unwrap();
        let e = p.parse_expr().unwrap();
        let resolve = |n: &str| COLS.iter().position(|c| *c == n);
        Compiler { resolve: &resolve, rules: RULES }.compile(&e).unwrap()
    }

    fn ctx() -> EvalCtx {
        EvalCtx { now_us: 0 }
    }

    #[test]
    fn three_valued_logic() {
        let e = compile("(a IS NOT NULL) + (b IS NOT NULL) + (c IS NOT NULL) IN (0, 3)");
        let row = |a: Value, b: Value, c: Value| vec![a, b, c, Value::Null, Value::Null, Value::Null, Value::Null];
        assert!(holds(&e, &row(Value::Int(1), Value::Int(1), Value::Int(1)), &ctx()));
        assert!(holds(&e, &row(Value::Null, Value::Null, Value::Null), &ctx()));
        assert!(!holds(&e, &row(Value::Int(1), Value::Null, Value::Null), &ctx()));
        let gt = compile("a > b");
        assert!(!holds(&gt, &row(Value::Null, Value::Int(1), Value::Null), &ctx()), "NULL is not true");
        let either = compile("end_date IS NULL OR end_date > start_date");
        let mut r = vec![Value::Null; 7];
        r[5] = Value::Time(10);
        assert!(holds(&either, &r, &ctx()));
        r[4] = Value::Time(5);
        assert!(!holds(&either, &r, &ctx()));
    }

    #[test]
    fn case_and_functions() {
        let e = compile("CASE status WHEN 'BED' THEN a IS NOT NULL WHEN 'ROOM' THEN b IS NOT NULL ELSE false END");
        let mut r = vec![Value::Null; 7];
        r[3] = Value::Str("BED".into());
        r[0] = Value::Int(1);
        assert!(holds(&e, &r, &ctx()));
        r[3] = Value::Str("OTHER".into());
        assert!(!holds(&e, &r, &ctx()));
        let l = compile("length(status) <= 3 AND lower(status) LIKE 'o%'");
        r[3] = Value::Str("Ok".into());
        assert!(holds(&l, &r, &ctx()));
        let w = compile("a + b + c = 1");
        let mut d = vec![Value::Decimal(100, 3), Value::Decimal(700, 3), Value::Decimal(200, 3), Value::Null, Value::Null, Value::Null, Value::Null];
        assert!(holds(&w, &d, &ctx()));
        d[2] = Value::Decimal(201, 3);
        assert!(!holds(&w, &d, &ctx()));
    }

    #[test]
    fn analysis() {
        let mut hints = Vec::new();
        compile("CASE status WHEN 'BED' THEN 1 WHEN 'ROOM' THEN 2 END = 1").literal_hints(&mut hints);
        assert_eq!(hints.iter().map(|(_, v)| v.to_string()).collect::<Vec<_>>(), ["'BED'", "'ROOM'"]);
        assert_eq!(compile("a > 0").bounds(), vec![(0, CmpOp::Gt, Value::Int(0))]);
        assert_eq!(compile("10 >= a").bounds(), vec![(0, CmpOp::Le, Value::Int(10))]);
        assert_eq!(compile("status IN ('A', 'B')").closed_domain().unwrap().1.len(), 2);
        let assign = compile("is_current = (end_date IS NULL)").assignments();
        assert_eq!(assign[0].0, 6);
        let linear = compile("a + b + c = 1").assignments();
        let mut row = vec![Value::Decimal(250, 3), Value::Decimal(500, 3), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null];
        let (col, rhs) = linear.iter().find(|(c, _)| *c == 2).unwrap();
        row[*col] = cast(eval(rhs, &row, &ctx()), &DataType::Decimal { precision: 4, scale: 3 });
        assert_eq!(row[2], Value::Decimal(250, 3));
        assert_eq!(compile("end_date IS NULL OR end_date > start_date").ordering(), Some((4, 5, true)));
        assert_eq!(compile("start_date <= end_date").ordering(), Some((4, 5, false)));
    }
}

#[cfg(test)]
mod clickhouse_function_tests {
    use super::*;
    use sqlparser::dialect::ClickHouseDialect;
    use sqlparser::parser::Parser;

    fn compile(sql: &str) -> CExpr {
        let names = ["channel", "store_id"];
        let resolve = |n: &str| names.iter().position(|c| *c == n);
        let expr = Parser::new(&ClickHouseDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap();
        Compiler { resolve: &resolve, rules: crate::sql::convert::TypeRules::STANDARD }.compile(&expr).unwrap()
    }

    /// ClickHouse stores `CASE x WHEN ...` as caseWithExpression: it must mean the same.
    #[test]
    fn case_with_expression_is_a_case() {
        let stored = compile("caseWithExpression(channel, 'store', store_id IS NOT NULL, store_id IS NULL)");
        let written = compile("CASE channel WHEN 'store' THEN store_id IS NOT NULL ELSE store_id IS NULL END");
        assert!(matches!(stored, CExpr::Case { operand: Some(_), .. }));
        let ctx = EvalCtx { now_us: 0 };
        for row in [
            vec![Value::Str("store".into()), Value::Int(1)],
            vec![Value::Str("store".into()), Value::Null],
            vec![Value::Str("web".into()), Value::Int(1)],
            vec![Value::Str("web".into()), Value::Null],
        ] {
            assert_eq!(holds(&stored, &row, &ctx), holds(&written, &row, &ctx), "{row:?}");
        }
    }
}
