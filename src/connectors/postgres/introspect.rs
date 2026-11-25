//! The DDL of a live PostgreSQL schema, rebuilt from its catalogs, for
//! [`Backend::introspect_ddl`](crate::db::Backend::introspect_ddl).
//!
//! PostgreSQL keeps no DDL text, so the script is reassembled from `pg_type`,
//! `pg_attribute`, `pg_constraint` and `pg_index`, with PostgreSQL's own deparsers
//! (`format_type`, `pg_get_expr`, `pg_get_constraintdef`, `pg_get_indexdef`). Where the
//! deparsed form compiles to something other than what DDL usually declares, it is
//! written back in the declared form: `timestamptz`, `serial`, and CHECKs without the
//! casts and `= ANY (ARRAY[...])` PostgreSQL adds, so dbgen still reads their bounds and
//! value lists.

use sqlparser::ast::{BinaryOperator, CaseWhen, CastKind, DataType as S, Expr, InTarget};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use crate::db::{DbResult, TextRows};

/// Enum types and their labels, in creation order.
const ENUMS: &str = "SELECT quote_ident(t.typname), string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder)
    FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace JOIN pg_enum e ON e.enumtypid = t.oid
    WHERE n.nspname = current_schema() GROUP BY t.oid, t.typname ORDER BY t.oid";

/// Domains: base type, NOT NULL, default, checks.
const DOMAINS: &str = "SELECT quote_ident(t.typname), format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault,
        (SELECT string_agg(pg_get_constraintdef(c.oid), ' ' ORDER BY c.oid) FROM pg_constraint c WHERE c.contypid = t.oid AND c.contype = 'c')
    FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
    WHERE t.typtype = 'd' AND n.nspname = current_schema() ORDER BY t.oid";

/// Tables and partitioned tables of the current schema, without the partitions, in
/// creation order, with their persistence and partition key.
const TABLES: &str = "SELECT c.oid, quote_ident(c.relname), c.relpersistence, CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') AND NOT c.relispartition ORDER BY c.oid";

/// Columns: table, name, type, NOT NULL, identity, generated, default expression, and
/// whether the column is a serial (its default draws from a sequence it owns).
const COLUMNS: &str = "SELECT a.attrelid, quote_ident(a.attname), format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attidentity, a.attgenerated,
        pg_get_expr(d.adbin, d.adrelid),
        COALESCE(a.attidentity = '' AND a.attgenerated = '' AND pg_get_expr(d.adbin, d.adrelid) LIKE 'nextval(%'
            AND pg_get_serial_sequence(a.attrelid::regclass::text, a.attname) IS NOT NULL, false)
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') AND NOT c.relispartition AND a.attnum > 0 AND NOT a.attisdropped
    ORDER BY a.attrelid, a.attnum";

/// Primary keys, uniques, checks and foreign keys declared on the tables themselves
/// (not the copies PostgreSQL makes for partitions), primary key first.
const CONSTRAINTS: &str = "SELECT co.conrelid, co.contype, quote_ident(co.conname), pg_get_constraintdef(co.oid)
    FROM pg_constraint co JOIN pg_class c ON c.oid = co.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') AND NOT c.relispartition AND co.contype IN ('p', 'u', 'c', 'f') AND co.conparentid = 0
    ORDER BY co.conrelid, CASE co.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 ELSE 3 END, co.oid";

/// Unique indexes that back no constraint (`CREATE UNIQUE INDEX`).
const UNIQUE_INDEXES: &str = "SELECT pg_get_indexdef(i.indexrelid)
    FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') AND NOT c.relispartition AND i.indisunique
        AND NOT EXISTS (SELECT 1 FROM pg_constraint co WHERE co.conindid = i.indexrelid AND co.conrelid = i.indrelid AND co.contype IN ('p', 'u', 'x'))
    ORDER BY i.indexrelid";

/// The DDL of `current_schema()`, reading the catalogs with `query`: enums, domains,
/// tables (columns with identity, generated and default clauses; primary keys, uniques
/// and checks; the partition key of a partitioned table, whose partitions are left out),
/// unique indexes that back no constraint, and last the foreign keys, as `ALTER TABLE
/// ... ADD CONSTRAINT`, so tables may come in any order.
pub(super) fn ddl(query: impl Fn(&str) -> DbResult<TextRows>) -> DbResult<String> {
    let mut out = String::new();
    for row in query(ENUMS)? {
        if let [Some(name), Some(labels)] = row.as_slice() {
            out.push_str(&format!("CREATE TYPE {name} AS ENUM ({labels});\n"));
        }
    }
    for row in query(DOMAINS)? {
        if let [Some(name), Some(base), Some(not_null), default, checks] = row.as_slice() {
            out.push_str(&format!("CREATE DOMAIN {name} AS {}", declared_type(base)));
            if let Some(d) = default {
                out.push_str(&format!(" DEFAULT {d}"));
            }
            if not_null == "t" {
                out.push_str(" NOT NULL");
            }
            if let Some(c) = checks {
                out.push_str(&format!(" {c}"));
            }
            out.push_str(";\n");
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    let columns = query(COLUMNS)?;
    let constraints = query(CONSTRAINTS)?;
    let mut foreign_keys = String::new();
    for row in query(TABLES)? {
        let [Some(oid), Some(table), Some(persistence), partition_key] = row.as_slice() else { continue };
        let of_table = |r: &&Vec<Option<String>>| r.first().and_then(Option::as_ref) == Some(oid);
        let mut items: Vec<String> = columns.iter().filter(of_table).filter_map(|c| column_ddl(&c[1..])).collect();
        for c in constraints.iter().filter(of_table) {
            if let [_, Some(kind), Some(name), Some(def)] = c.as_slice() {
                match kind.as_str() {
                    "f" => foreign_keys.push_str(&format!("ALTER TABLE {table} ADD CONSTRAINT {name} {def};\n")),
                    "c" => items.push(format!("CONSTRAINT {name} {}", declared_check(def))),
                    _ => items.push(format!("CONSTRAINT {name} {def}")),
                }
            }
        }
        let unlogged = if persistence == "u" { "UNLOGGED " } else { "" };
        out.push_str(&format!("CREATE {unlogged}TABLE {table} (\n    {}\n)", items.join(",\n    ")));
        if let Some(key) = partition_key {
            out.push_str(&format!(" PARTITION BY {key}"));
        }
        out.push_str(";\n\n");
    }
    for row in query(UNIQUE_INDEXES)? {
        if let Some(Some(def)) = row.into_iter().next() {
            // An index of a partitioned table is declared `ON ONLY` the parent.
            out.push_str(&format!("{};\n", def.replacen(" ON ONLY ", " ON ", 1)));
        }
    }
    out.push_str(&foreign_keys);
    Ok(out)
}

/// One column definition from a row of [`COLUMNS`] (without the table).
fn column_ddl(row: &[Option<String>]) -> Option<String> {
    let [Some(name), Some(ty), Some(not_null), Some(identity), Some(generated), default, Some(serial)] = row else { return None };
    let mut def = match (serial.as_str(), ty.as_str()) {
        // A serial reads back as a sequence default; declared as serial, it is an identity.
        ("t", "smallint") => format!("{name} smallserial"),
        ("t", "integer") => format!("{name} serial"),
        ("t", "bigint") => format!("{name} bigserial"),
        _ => format!("{name} {}", declared_type(ty)),
    };
    match (identity.as_str(), generated.as_str(), default) {
        ("a", _, _) => def.push_str(" GENERATED ALWAYS AS IDENTITY"),
        ("d", _, _) => def.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
        (_, "s", Some(e)) => def.push_str(&format!(" GENERATED ALWAYS AS ({e}) STORED")),
        (_, "v", Some(e)) => def.push_str(&format!(" GENERATED ALWAYS AS ({e}) VIRTUAL")),
        (_, _, Some(e)) if serial != "t" => def.push_str(&format!(" DEFAULT {e}")),
        _ => {}
    }
    if not_null == "t" {
        def.push_str(" NOT NULL");
    }
    Some(def)
}

/// A type as `format_type` spells it, in the spelling DDL usually declares: `timestamptz`
/// for `timestamp with time zone`, which dbgen reads as a UTC timestamp.
fn declared_type(ty: &str) -> String {
    match ty.strip_prefix("timestamp with time zone") {
        Some(rest) => format!("timestamptz{rest}"),
        None => ty.to_string(),
    }
}

/// A CHECK as `pg_get_constraintdef` deparses it, in the form DDL declares it:
/// `(x)::text = ANY (ARRAY['a'::text, 'b'::text])` becomes `x IN ('a', 'b')`, and
/// `d < '2008-01-01'::date` becomes `d < '2008-01-01'`. Casts that change a value are
/// kept. A CHECK that does not parse, or has trailing options (`NOT VALID`), is kept as
/// it is.
fn declared_check(def: &str) -> String {
    let Some(body) = def.strip_prefix("CHECK ") else { return def.to_string() };
    let Ok(mut parser) = Parser::new(&PostgreSqlDialect {}).try_with_sql(body) else { return def.to_string() };
    let Ok(expr) = parser.parse_expr() else { return def.to_string() };
    if parser.peek_token().token != Token::EOF {
        return def.to_string();
    }
    format!("CHECK ({})", unnested(&plain(expr)))
}

/// Removes what PostgreSQL adds when it deparses an expression (see [`declared_check`]).
fn plain(e: Expr) -> Expr {
    let bx = |e: Expr| Box::new(plain(e));
    match e {
        Expr::Nested(x) => Expr::Nested(bx(*x)),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp { left: bx(*left), op, right: bx(*right) },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp { op, expr: bx(*expr) },
        Expr::IsNull(x) => Expr::IsNull(bx(*x)),
        Expr::IsNotNull(x) => Expr::IsNotNull(bx(*x)),
        Expr::Between { expr, negated, low, high } => Expr::Between { expr: bx(*expr), negated, low: bx(*low), high: bx(*high) },
        Expr::In { expr, target: InTarget::List(list), negated, global } => {
            Expr::In { expr: bx(*expr), target: InTarget::List(list.into_iter().map(plain).collect()), negated, global }
        }
        Expr::Case { case_token, end_token, operand, conditions, else_result } => Expr::Case {
            case_token,
            end_token,
            operand: operand.map(|o| bx(*o)),
            conditions: conditions.into_iter().map(|w| CaseWhen { condition: plain(w.condition), result: plain(w.result) }).collect(),
            else_result: else_result.map(|e| bx(*e)),
        },
        // `x = ANY (ARRAY[...])` is `x IN (...)`, `x <> ALL (ARRAY[...])` is `x NOT IN (...)`.
        Expr::AnyOp { left, compare_op: BinaryOperator::Eq, right, .. } if array(&right).is_some() => {
            Expr::In { expr: bx(*left), target: InTarget::List(array(&right).unwrap_or_default().into_iter().map(plain).collect()), negated: false, global: false }
        }
        Expr::AllOp { left, compare_op: BinaryOperator::NotEq, right } if array(&right).is_some() => {
            Expr::In { expr: bx(*left), target: InTarget::List(array(&right).unwrap_or_default().into_iter().map(plain).collect()), negated: true, global: false }
        }
        Expr::Cast { kind: CastKind::DoubleColon, expr, data_type, format: None } => match unnested(&expr) {
            // A literal is compared as the column's type anyway.
            Expr::Value(v) => Expr::Value(v.clone()),
            // A text column cast to text is itself.
            x @ (Expr::Identifier(_) | Expr::CompoundIdentifier(_)) if is_text(&data_type) => x.clone(),
            _ => Expr::Cast { kind: CastKind::DoubleColon, expr: bx(*expr), data_type, format: None },
        },
        other => other,
    }
}

/// The elements of `ARRAY[...]`, through parentheses and casts.
fn array(e: &Expr) -> Option<Vec<Expr>> {
    match unnested(e) {
        Expr::Array(a) => Some(a.elem.clone()),
        Expr::Cast { expr, .. } => array(expr),
        _ => None,
    }
}

fn unnested(e: &Expr) -> &Expr {
    match e {
        Expr::Nested(x) => unnested(x),
        other => other,
    }
}

/// Whether a cast to `t` leaves a text column as it is.
fn is_text(t: &S) -> bool {
    match t {
        S::Text | S::Varchar(_) | S::CharacterVarying(_) | S::Char(_) | S::Character(_) => true,
        S::Custom(name, _) => name.to_string().eq_ignore_ascii_case("bpchar"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_read_as_declared() {
        assert_eq!(declared_check("CHECK (((hours > (0)::numeric) AND (hours <= (24)::numeric)))"), "CHECK ((hours > 0) AND (hours <= 24))");
        assert_eq!(
            declared_check("CHECK (((channel)::text = ANY ((ARRAY['store'::character varying, 'web'::character varying])::text[])))"),
            "CHECK (channel IN ('store', 'web'))"
        );
        assert_eq!(declared_check("CHECK ((status <> ALL (ARRAY['x'::text, 'y'::text])))"), "CHECK (status NOT IN ('x', 'y'))");
        assert_eq!(declared_check("CHECK ((birth_date < '2008-01-01'::date))"), "CHECK (birth_date < '2008-01-01')");
        // Casts that change the value stay; options after the expression keep the text.
        assert_eq!(declared_check("CHECK (((created_at)::date <= shipped_on))"), "CHECK ((created_at)::DATE <= shipped_on)");
        assert_eq!(declared_check("CHECK ((n > 0)) NOT VALID"), "CHECK ((n > 0)) NOT VALID");
    }

    #[test]
    fn columns_read_as_declared() {
        let col = |cells: [&str; 7]| column_ddl(&cells.map(|c| (c != "-").then(|| c.to_string()))).unwrap();
        assert_eq!(col(["id", "integer", "t", "", "", "nextval('t_id_seq'::regclass)", "t"]), "id serial NOT NULL");
        assert_eq!(col(["id", "bigint", "t", "a", "", "-", "f"]), "id bigint GENERATED ALWAYS AS IDENTITY NOT NULL");
        assert_eq!(col(["at", "timestamp with time zone", "f", "", "", "now()", "f"]), "at timestamptz DEFAULT now()");
        assert_eq!(col(["n", "text", "f", "", "s", "upper(a)", "f"]), "n text GENERATED ALWAYS AS (upper(a)) STORED");
        assert_eq!(declared_type("timestamp with time zone[]"), "timestamptz[]");
        assert_eq!(declared_type("timestamp(3) with time zone"), "timestamp(3) with time zone");
    }
}
