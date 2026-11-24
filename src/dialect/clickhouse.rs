//! ClickHouse: the adapter that drives `sqlparser` over ClickHouse DDL, and the semantics
//! of ClickHouse table engines.
//!
//! `sqlparser` parses ClickHouse types, column definitions and expressions, but not the
//! structure of ClickHouse `CREATE TABLE` and `ALTER TABLE` statements: table clauses
//! in any order, `COMMENT` on the table, `CONSTRAINT ... CHECK` without parentheses,
//! `INDEX`/`PROJECTION` elements, `CODEC`/`TTL` on columns, `ADD COLUMN IF NOT EXISTS`,
//! `MODIFY COLUMN`... This module walks those statements on `sqlparser`'s own tokens and
//! hands every column definition, type and expression to `sqlparser`.

use std::ops::Range;

use sqlparser::ast::{Expr, Function, FunctionArguments};
use sqlparser::dialect::ClickHouseDialect;
use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::schema::{DataType, Key, KeyKind, KeyPart};
use crate::sql::convert::{self, Constraint, TypeRules};
use crate::sql::{
    are_kws, is_kw, matching, parse_slice, parse_tokens, read_name, split_commas, string_lit, top_level_positions, word, AlterAction,
    CheckSpec, ColumnPosition, ColumnSpec, Ddl, DefaultKind, DefaultSpec, Diag, EngineSpec, ExprSpec, Statement, TableDef,
};

use super::TableSemantics;

pub static DIALECT: ClickHouseDialect = ClickHouseDialect {};

pub const RULES: TypeRules = TypeRules {
    int8_is_8_bits: true,
    nullable_by_default: false,
    varchar_has_length: false,
    date_is_16_bit: true,
    timestamp_is_seconds: true,
};

/// Column-definition keywords that end a type or a `TTL` expression.
const COLUMN_OPTION_KEYWORDS: &[&str] = &[
    "NULL", "NOT", "DEFAULT", "MATERIALIZED", "ALIAS", "EPHEMERAL", "COMMENT", "CODEC", "TTL", "SETTINGS", "STATISTICS", "PRIMARY",
    "FIRST", "AFTER", "REMOVE", "MODIFY", "RESET",
];

pub fn parse_statement(stmt: &Statement) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    if is_kw(t.first(), "CREATE") {
        let mut i = 1;
        let mut or_replace = false;
        if are_kws(t, i, &["OR", "REPLACE"]) {
            or_replace = true;
            i += 2;
        }
        if is_kw(t.get(i), "TEMPORARY") {
            i += 1;
        }
        if is_kw(t.get(i), "TABLE") {
            return create_table(stmt, i + 1, or_replace);
        }
        if are_kws(t, i, &["MATERIALIZED", "VIEW"]) {
            return materialized_view(stmt, i + 2);
        }
    }
    if are_kws(t, 0, &["ALTER", "TABLE"]) {
        return alter_table(stmt, 2);
    }
    if is_kw(t.first(), "DROP") {
        let i = if is_kw(t.get(1), "TEMPORARY") { 2 } else { 1 };
        if is_kw(t.get(i), "TABLE") {
            return drop_tables(stmt, i + 1);
        }
    }
    if are_kws(t, 0, &["RENAME", "TABLE"]) {
        return rename_tables(stmt, 2);
    }
    if are_kws(t, 0, &["EXCHANGE", "TABLES"]) {
        let (a, i) = read_name(t, 2).ok_or_else(|| stmt.diag(stmt.loc(2), "expected a table name"))?;
        if !is_kw(t.get(i), "AND") {
            return Err(stmt.diag(stmt.loc(i), "expected AND"));
        }
        let (b, _) = read_name(t, i + 1).ok_or_else(|| stmt.diag(stmt.loc(i + 1), "expected a table name"))?;
        return Ok(Ddl::ExchangeTables(a, b));
    }
    if are_kws(t, 0, &["INSERT", "INTO"]) {
        let mut i = 2;
        if is_kw(t.get(i), "TABLE") {
            i += 1;
        }
        if !is_kw(t.get(i), "FUNCTION") {
            if let Some((table, _)) = read_name(t, i) {
                return Ok(Ddl::Insert { table });
            }
        }
    }
    Ok(Ddl::Ignored(leading_words(stmt, 2)))
}

fn leading_words(stmt: &Statement, n: usize) -> String {
    stmt.toks.iter().take(n).map_while(|t| word(Some(t))).map(|w| w.to_uppercase()).collect::<Vec<_>>().join(" ")
}

fn expected(stmt: &Statement, i: usize, what: &str) -> Diag {
    let found = stmt.toks.get(i).map_or("end of statement".to_string(), |t| format!("'{}'", t.token));
    stmt.diag(stmt.loc(i), format!("expected {what}, found {found}"))
}

/// Skips `ON CLUSTER name` at `i`.
fn skip_on_cluster(t: &[TokenWithSpan], i: usize) -> usize {
    if are_kws(t, i, &["ON", "CLUSTER"]) {
        i + 3
    } else {
        i
    }
}

pub fn expr_at(stmt: &Statement, range: Range<usize>, what: &str) -> Result<ExprSpec, Diag> {
    let sql = stmt.slice_text(range.clone());
    let expr = parse_slice(stmt, range, &DIALECT, what, |p| p.parse_expr())?;
    Ok(ExprSpec { expr, sql })
}

// ---------------------------------------------------------------------------------------
// CREATE TABLE
// ---------------------------------------------------------------------------------------

fn create_table(stmt: &Statement, mut i: usize, or_replace: bool) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    let mut def = TableDef { or_replace, at: stmt.source.at(stmt.start()), ..TableDef::default() };
    if are_kws(t, i, &["IF", "NOT", "EXISTS"]) {
        def.if_not_exists = true;
        i += 3;
    }
    let (name, next) = read_name(t, i).ok_or_else(|| expected(stmt, i, "a table name"))?;
    def.name = name;
    i = next;
    if is_kw(t.get(i), "UUID") {
        i += 2;
    }
    i = skip_on_cluster(t, i);
    if matches!(t.get(i).map(|t| &t.token), Some(Token::LParen)) {
        let close = matching(t, i).ok_or_else(|| stmt.diag(stmt.loc(i), "unbalanced parenthesis"))?;
        elements(stmt, i + 1..close, &mut def)?;
        i = close + 1;
    }
    let like = clauses(stmt, i..t.len(), &mut def)?;
    match like {
        Some(source) => Ok(Ddl::CreateTableLike { def, source }),
        None if def.columns.is_empty() => Ok(Ddl::Ignored("CREATE TABLE ... AS SELECT (columns come from a query)".into())),
        None => Ok(Ddl::CreateTable(def)),
    }
}

/// The column list: columns, constraints, indexes, projections.
fn elements(stmt: &Statement, range: Range<usize>, def: &mut TableDef) -> Result<(), Diag> {
    let t = &stmt.toks;
    for r in split_commas(t, range) {
        let s = r.start;
        if is_kw(t.get(s), "CONSTRAINT") {
            let name = word(t.get(s + 1)).ok_or_else(|| expected(stmt, s + 1, "a constraint name"))?;
            if is_kw(t.get(s + 2), "CHECK") {
                let expr = expr_at(stmt, s + 3..r.end, "CHECK expression")?;
                def.checks.push(CheckSpec { name: Some(name), expr });
                continue;
            }
            if is_kw(t.get(s + 2), "ASSUME") {
                continue; // not enforced: a hint to the optimizer
            }
            standard_constraint(stmt, r, def)?;
            continue;
        }
        if is_kw(t.get(s), "INDEX") && top_level_positions(t, r.clone()).iter().any(|&p| is_kw(t.get(p), "TYPE")) {
            continue;
        }
        if is_kw(t.get(s), "PROJECTION") && matches!(t.get(s + 2).map(|t| &t.token), Some(Token::LParen)) {
            continue;
        }
        if are_kws(t, s, &["PRIMARY", "KEY"]) {
            continue; // in ClickHouse a prefix of the sorting key, not a uniqueness constraint
        }
        if is_kw(t.get(s), "UNIQUE") || are_kws(t, s, &["FOREIGN", "KEY"]) || is_kw(t.get(s), "CHECK") {
            standard_constraint(stmt, r, def)?;
            continue;
        }
        let (columns, _) = column(stmt, r, false)?;
        for c in columns {
            add_column_spec(def, c);
        }
    }
    Ok(())
}

fn add_column_spec(def: &mut TableDef, mut c: ColumnSpec) {
    if let Some(check) = c.check.take() {
        def.checks.push(check);
    }
    if let Some(fk) = c.references.take() {
        def.foreign_keys.push(fk);
    }
    if c.primary_key {
        def.primary_key = Some(vec![c.name.clone()]);
    }
    if c.unique {
        def.uniques.push(vec![c.name.clone()]);
    }
    def.columns.push(c);
}

fn standard_constraint(stmt: &Statement, r: Range<usize>, def: &mut TableDef) -> Result<(), Diag> {
    let c = parse_slice(stmt, r.clone(), &DIALECT, "table constraint", |p| p.parse_optional_table_constraint())?
        .ok_or_else(|| expected(stmt, r.start, "a table constraint"))?;
    match convert::table_constraint(stmt, c) {
        Constraint::Check(c) => def.checks.push(c),
        Constraint::PrimaryKey(cols) => def.primary_key = Some(cols),
        Constraint::Unique(cols) => def.uniques.push(cols),
        Constraint::ForeignKey(fk) => def.foreign_keys.push(fk),
        Constraint::Other => {}
    }
    Ok(())
}

/// One column definition. ClickHouse-only parts `sqlparser` does not parse are taken out
/// first: `CODEC(...)`, `STATISTICS(...)`, `SETTINGS (...)`, `TTL expr`, a bare
/// `EPHEMERAL`, the time zone of `DateTime('tz')`, and (in ALTER) `FIRST`/`AFTER name`.
fn column(stmt: &Statement, r: Range<usize>, in_alter: bool) -> Result<(Vec<ColumnSpec>, Option<ColumnPosition>), Diag> {
    let t = &stmt.toks;
    let mut keep: Vec<TokenWithSpan> = Vec::with_capacity(r.len());
    let mut tz: Option<String> = None;
    let mut bare_ephemeral = false;
    let mut position = None;
    let mut depth = 0i32;
    let mut k = r.start;
    let is_option = |i: usize| COLUMN_OPTION_KEYWORDS.iter().any(|kw| is_kw(t.get(i), kw));
    while k < r.end {
        let tok = &t[k];
        // DateTime('tz'), possibly nested in Nullable(...) or LowCardinality(...).
        if is_kw(Some(tok), "DateTime")
            && matches!(t.get(k + 1).map(|t| &t.token), Some(Token::LParen))
            && string_lit(t.get(k + 2)).is_some()
            && matches!(t.get(k + 3).map(|t| &t.token), Some(Token::RParen))
        {
            tz = string_lit(t.get(k + 2));
            keep.push(tok.clone());
            k += 4;
            continue;
        }
        if depth == 0 && k > r.start + 1 {
            if (is_kw(Some(tok), "CODEC") || is_kw(Some(tok), "STATISTICS") || is_kw(Some(tok), "SETTINGS"))
                && matches!(t.get(k + 1).map(|t| &t.token), Some(Token::LParen))
            {
                k = matching(t, k + 1).map_or(r.end, |c| c + 1);
                continue;
            }
            if is_kw(Some(tok), "TTL") {
                k += 1;
                let mut d = 0i32;
                while k < r.end && !(d == 0 && is_option(k)) {
                    match t[k].token {
                        Token::LParen => d += 1,
                        Token::RParen => d -= 1,
                        _ => {}
                    }
                    k += 1;
                }
                continue;
            }
            if is_kw(Some(tok), "EPHEMERAL") && (k + 1 >= r.end || is_option(k + 1)) {
                bare_ephemeral = true;
                k += 1;
                continue;
            }
            if in_alter && is_kw(Some(tok), "FIRST") {
                position = Some(ColumnPosition::First);
                k += 1;
                continue;
            }
            if in_alter && is_kw(Some(tok), "AFTER") {
                let after = word(t.get(k + 1)).ok_or_else(|| expected(stmt, k + 1, "a column name after AFTER"))?;
                position = Some(ColumnPosition::After(after));
                k += 2;
                continue;
            }
        }
        match tok.token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            _ => {}
        }
        keep.push(tok.clone());
        k += 1;
    }
    let def = parse_tokens(stmt, keep, &DIALECT, "column definition", |p| p.parse_column_def())?;
    let mut specs = convert::column(stmt, def, &RULES);
    for spec in &mut specs {
        if let Some(tz) = &tz {
            set_timezone(&mut spec.ty, tz);
        }
        if bare_ephemeral {
            spec.default = Some(DefaultSpec { kind: DefaultKind::Ephemeral, expr: None });
        }
    }
    Ok((specs, position))
}

fn set_timezone(ty: &mut DataType, zone: &str) {
    match ty {
        DataType::DateTime { precision: None, tz } if tz.is_none() => *tz = Some(zone.to_string()),
        DataType::Nullable(inner) | DataType::LowCardinality(inner) | DataType::Array(inner) => set_timezone(inner, zone),
        _ => {}
    }
}

/// Table clauses after the column list, in any order. Returns the source table of
/// `AS other_table`.
fn clauses(stmt: &Statement, range: Range<usize>, def: &mut TableDef) -> Result<Option<String>, Diag> {
    let t = &stmt.toks;
    // (start of keyword, start of body, clause)
    let mut starts: Vec<(usize, usize, &str)> = Vec::new();
    for p in top_level_positions(t, range.clone()) {
        let clause = if are_kws(t, p, &["ORDER", "BY"]) {
            Some(("ORDER BY", 2))
        } else if are_kws(t, p, &["PARTITION", "BY"]) {
            Some(("PARTITION BY", 2))
        } else if are_kws(t, p, &["PRIMARY", "KEY"]) {
            Some(("PRIMARY KEY", 2))
        } else if are_kws(t, p, &["SAMPLE", "BY"]) {
            Some(("SAMPLE BY", 2))
        } else {
            ["ENGINE", "TTL", "SETTINGS", "COMMENT", "AS", "EMPTY"].iter().find(|kw| is_kw(t.get(p), kw)).map(|kw| (*kw, 1))
        };
        if let Some((name, len)) = clause {
            // Everything after `AS SELECT ...` belongs to the query.
            if let Some(&(_, body, "AS")) = starts.last() {
                let b = t.get(body);
                if is_kw(b, "SELECT") || is_kw(b, "WITH") || matches!(b.map(|t| &t.token), Some(Token::LParen)) {
                    break;
                }
            }
            starts.push((p, p + len, name));
        }
    }
    if let Some(&(first, _, _)) = starts.first() {
        if first != range.start {
            return Err(expected(stmt, range.start, "a table clause (ENGINE, ORDER BY, PARTITION BY, SETTINGS, COMMENT...)"));
        }
    } else if !range.is_empty() {
        return Err(expected(stmt, range.start, "a table clause (ENGINE, ORDER BY, PARTITION BY, SETTINGS, COMMENT...)"));
    }
    let mut like = None;
    for (n, &(_, body, name)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(range.end, |s| s.0);
        let body = body..end;
        match name {
            "ENGINE" => def.engine = Some(engine(stmt, body)?),
            "ORDER BY" => def.order_by = tuple_items(expr_at(stmt, body, "ORDER BY expression")?),
            "PARTITION BY" => def.partition_by = Some(expr_at(stmt, body, "PARTITION BY expression")?),
            "COMMENT" => {
                let mut b = body.start;
                if matches!(t.get(b).map(|t| &t.token), Some(Token::Eq)) {
                    b += 1;
                }
                def.comment = Some(string_lit(t.get(b)).ok_or_else(|| expected(stmt, b, "a string after COMMENT"))?);
            }
            "AS" => {
                let b = body.start;
                let is_query = is_kw(t.get(b), "SELECT") || is_kw(t.get(b), "WITH") || matches!(t.get(b).map(|t| &t.token), Some(Token::LParen));
                if !is_query {
                    like = read_name(t, b).map(|(n, _)| n);
                }
            }
            _ => {} // PRIMARY KEY, SAMPLE BY, TTL, SETTINGS, EMPTY: no effect on the data
        }
    }
    Ok(like)
}

fn engine(stmt: &Statement, body: Range<usize>) -> Result<EngineSpec, Diag> {
    let t = &stmt.toks;
    let mut i = body.start;
    if matches!(t.get(i).map(|t| &t.token), Some(Token::Eq)) {
        i += 1;
    }
    let name = word(t.get(i)).ok_or_else(|| expected(stmt, i, "an engine name"))?;
    let mut args = Vec::new();
    if matches!(t.get(i + 1).map(|t| &t.token), Some(Token::LParen)) {
        let close = matching(t, i + 1).ok_or_else(|| stmt.diag(stmt.loc(i + 1), "unbalanced parenthesis"))?;
        for r in split_commas(t, i + 2..close) {
            args.push(expr_at(stmt, r, "engine argument")?);
        }
    }
    Ok(EngineSpec { name, args })
}

/// `(a, b)` and `tuple(a, b)` -> `[a, b]`; `tuple()` -> `[]`; `a` -> `[a]`.
fn tuple_items(e: ExprSpec) -> Vec<ExprSpec> {
    let items = match &e.expr {
        Expr::Tuple(items) => Some(items.clone()),
        Expr::Nested(inner) => Some(vec![(**inner).clone()]),
        Expr::Function(Function { name, args: FunctionArguments::List(list), .. }) if name.to_string().eq_ignore_ascii_case("tuple") => {
            Some(list.args.iter().filter_map(|a| match a {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => Some(e.clone()),
                _ => None,
            }).collect())
        }
        _ => None,
    };
    match items {
        Some(items) => items.into_iter().map(|expr| ExprSpec { sql: expr.to_string(), expr }).collect(),
        None => vec![e],
    }
}

fn materialized_view(stmt: &Statement, mut i: usize) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    if are_kws(t, i, &["IF", "NOT", "EXISTS"]) {
        i += 3;
    }
    let (name, next) = read_name(t, i).ok_or_else(|| expected(stmt, i, "a view name"))?;
    let mut target = None;
    for p in top_level_positions(t, next..t.len()) {
        if is_kw(t.get(p), "AS") {
            break;
        }
        if is_kw(t.get(p), "TO") {
            target = read_name(t, p + 1).map(|(n, _)| n);
            break;
        }
    }
    Ok(Ddl::MaterializedView { name, target })
}

// ---------------------------------------------------------------------------------------
// ALTER, DROP, RENAME
// ---------------------------------------------------------------------------------------

fn alter_table(stmt: &Statement, i: usize) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    let (table, next) = read_name(t, i).ok_or_else(|| expected(stmt, i, "a table name"))?;
    let start = skip_on_cluster(t, next);
    let mut actions = Vec::new();
    for r in split_commas(t, start..t.len()) {
        actions.push(alter_action(stmt, r)?);
    }
    Ok(Ddl::AlterTable { table, actions })
}

fn alter_action(stmt: &Statement, r: Range<usize>) -> Result<AlterAction, Diag> {
    let t = &stmt.toks;
    let s = r.start;
    let if_exists_at = |i: usize| if are_kws(t, i, &["IF", "EXISTS"]) { (true, i + 2) } else { (false, i) };
    let if_not_exists_at = |i: usize| if are_kws(t, i, &["IF", "NOT", "EXISTS"]) { (true, i + 3) } else { (false, i) };
    let name_at = |i: usize| word(t.get(i)).ok_or_else(|| expected(stmt, i, "a name"));

    if are_kws(t, s, &["ADD", "COLUMN"]) {
        let (if_not_exists, c) = if_not_exists_at(s + 2);
        let (mut columns, after) = column(stmt, c..r.end, true)?;
        if columns.len() != 1 {
            return Ok(AlterAction::Ignored("ADD COLUMN of a Nested type".into()));
        }
        return Ok(AlterAction::AddColumn { column: columns.remove(0), if_not_exists, after });
    }
    if are_kws(t, s, &["ADD", "CONSTRAINT"]) {
        let (_, n) = if_not_exists_at(s + 2);
        let name = name_at(n)?;
        if is_kw(t.get(n + 1), "CHECK") {
            let expr = expr_at(stmt, n + 2..r.end, "CHECK expression")?;
            return Ok(AlterAction::AddCheck(CheckSpec { name: Some(name), expr }));
        }
        return Ok(AlterAction::Ignored("ADD CONSTRAINT ... ASSUME".into()));
    }
    if are_kws(t, s, &["DROP", "COLUMN"]) {
        let (if_exists, n) = if_exists_at(s + 2);
        return Ok(AlterAction::DropColumn { name: name_at(n)?, if_exists });
    }
    if are_kws(t, s, &["DROP", "CONSTRAINT"]) {
        let (if_exists, n) = if_exists_at(s + 2);
        return Ok(AlterAction::DropConstraint { name: name_at(n)?, if_exists });
    }
    if are_kws(t, s, &["RENAME", "COLUMN"]) {
        let (if_exists, n) = if_exists_at(s + 2);
        let from = name_at(n)?;
        if !is_kw(t.get(n + 1), "TO") {
            return Err(expected(stmt, n + 1, "TO"));
        }
        return Ok(AlterAction::RenameColumn { from, to: name_at(n + 2)?, if_exists });
    }
    if are_kws(t, s, &["COMMENT", "COLUMN"]) {
        let (_, n) = if_exists_at(s + 2);
        let name = name_at(n)?;
        let comment = string_lit(t.get(n + 1)).ok_or_else(|| expected(stmt, n + 1, "a comment string"))?;
        return Ok(AlterAction::CommentColumn { name, comment });
    }
    if are_kws(t, s, &["MODIFY", "ORDER", "BY"]) {
        return Ok(AlterAction::ModifyOrderBy(tuple_items(expr_at(stmt, s + 3..r.end, "ORDER BY expression")?)));
    }
    if are_kws(t, s, &["MODIFY", "COMMENT"]) {
        let comment = string_lit(t.get(s + 2)).ok_or_else(|| expected(stmt, s + 2, "a comment string"))?;
        return Ok(AlterAction::ModifyComment(comment));
    }
    if are_kws(t, s, &["MODIFY", "COLUMN"]) {
        let (if_exists, n) = if_exists_at(s + 2);
        let name = name_at(n)?;
        let next = n + 1;
        if is_kw(t.get(next), "REMOVE") {
            let property = word(t.get(next + 1)).unwrap_or_default().to_uppercase();
            return Ok(AlterAction::RemoveColumnProperty { name, property });
        }
        if is_kw(t.get(next), "MODIFY") || is_kw(t.get(next), "RESET") {
            return Ok(AlterAction::Ignored("MODIFY COLUMN ... SETTING".into()));
        }
        let has_type = next < r.end && !COLUMN_OPTION_KEYWORDS.iter().any(|kw| is_kw(t.get(next), kw));
        if has_type {
            let (mut columns, _) = column(stmt, n..r.end, true)?;
            let c = columns.remove(0);
            return Ok(AlterAction::ModifyColumn { name, if_exists, ty: Some(c.ty), default: c.default, comment: c.comment });
        }
        // Options only: parse them as a column definition with a placeholder type.
        let mut toks = vec![t[n].clone()];
        let mut placeholder = t[n].clone();
        placeholder.token = Token::make_word("String", None);
        toks.push(placeholder);
        toks.extend(t[next..r.end].iter().cloned());
        let range_copy = Statement { source: stmt.source, toks: toks.clone(), range: stmt.range.clone() };
        let (columns, _) = column(&range_copy, 0..toks.len(), true)?;
        let c = columns.into_iter().next().ok_or_else(|| expected(stmt, n, "a column definition"))?;
        return Ok(AlterAction::ModifyColumn { name, if_exists, ty: None, default: c.default, comment: c.comment });
    }
    Ok(AlterAction::Ignored(leading_words_at(stmt, s, 2)))
}

fn leading_words_at(stmt: &Statement, s: usize, n: usize) -> String {
    stmt.toks[s..].iter().take(n).map_while(|t| word(Some(t))).map(|w| w.to_uppercase()).collect::<Vec<_>>().join(" ")
}

fn drop_tables(stmt: &Statement, mut i: usize) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    let mut if_exists = false;
    if are_kws(t, i, &["IF", "EXISTS"]) {
        if_exists = true;
        i += 2;
    }
    let mut names = Vec::new();
    loop {
        let (name, next) = read_name(t, i).ok_or_else(|| expected(stmt, i, "a table name"))?;
        names.push(name);
        i = next;
        if matches!(t.get(i).map(|t| &t.token), Some(Token::Comma)) {
            i += 1;
        } else {
            break;
        }
    }
    Ok(Ddl::DropTables { names, if_exists })
}

fn rename_tables(stmt: &Statement, i: usize) -> Result<Ddl, Diag> {
    let t = &stmt.toks;
    let end = top_level_positions(t, i..t.len()).into_iter().find(|&p| are_kws(t, p, &["ON", "CLUSTER"])).unwrap_or(t.len());
    let mut pairs = Vec::new();
    for r in split_commas(t, i..end) {
        let (from, n) = read_name(t, r.start).ok_or_else(|| expected(stmt, r.start, "a table name"))?;
        if !is_kw(t.get(n), "TO") {
            return Err(expected(stmt, n, "TO"));
        }
        let (to, _) = read_name(t, n + 1).ok_or_else(|| expected(stmt, n + 1, "a table name"))?;
        pairs.push((from, to));
    }
    Ok(Ddl::RenameTables(pairs))
}

// ---------------------------------------------------------------------------------------
// Engine semantics and types
// ---------------------------------------------------------------------------------------

/// Engines whose background merges collapse rows with equal sorting keys: for them the
/// sorting key is an identity, and two generated rows must never share it.
pub fn table_semantics(engine: Option<&EngineSpec>, order_by: &[ExprSpec]) -> TableSemantics {
    let mut sem = TableSemantics::default();
    let Some(engine) = engine else { return sem };
    let base = engine.name.trim_start_matches("Replicated").trim_start_matches("Shared");
    // Replicated engines take the coordination path and replica name first.
    let args: Vec<&ExprSpec> = engine
        .args
        .iter()
        .skip_while(|a| matches!(&a.expr, Expr::Value(v) if matches!(v.value, sqlparser::ast::Value::SingleQuotedString(_))))
        .collect();
    let column_arg = |i: usize| match args.get(i).map(|a| &a.expr) {
        Some(Expr::Identifier(id)) => Some(id.value.clone()),
        _ => None,
    };
    sem.collapsing = matches!(
        base,
        "ReplacingMergeTree"
            | "CollapsingMergeTree"
            | "VersionedCollapsingMergeTree"
            | "SummingMergeTree"
            | "AggregatingMergeTree"
            | "CoalescingMergeTree"
            | "GraphiteMergeTree"
    );
    match base {
        "ReplacingMergeTree" => {
            if let Some(is_deleted) = column_arg(1) {
                sem.fixed.push((is_deleted, 0));
            }
        }
        "CollapsingMergeTree" | "VersionedCollapsingMergeTree" => {
            if let Some(sign) = column_arg(0) {
                sem.fixed.push((sign, 1));
            }
        }
        _ => {}
    }
    if sem.collapsing && !order_by.is_empty() {
        sem.keys.push(Key {
            name: "sorting key".into(),
            kind: KeyKind::SortingKey,
            parts: order_by
                .iter()
                .map(|e| match &e.expr {
                    Expr::Identifier(id) => KeyPart::Column(id.value.clone()),
                    _ => KeyPart::Expr(e.sql.clone()),
                })
                .collect(),
        });
    }
    sem
}

/// Engines that store rows (as opposed to views, integrations, `Null`, `Distributed`...).
pub fn is_storage_table(def: &TableDef) -> bool {
    match &def.engine {
        None => true,
        Some(e) => {
            let n = e.name.as_str();
            n.ends_with("MergeTree") || matches!(n, "Log" | "TinyLog" | "StripeLog" | "Memory" | "Set" | "Join" | "EmbeddedRocksDB")
        }
    }
}

/// Parses a ClickHouse type string, as `system.columns` reports it.
pub fn parse_type(s: &str) -> Result<DataType, String> {
    // Parse it as the type of a dummy column, so DateTime('tz') goes through the adapter.
    let src = crate::sql::Source::new("<type>", format!("CREATE TABLE t (c {s})"));
    let stmts = crate::sql::split(&src, &DIALECT).map_err(|e| e.message)?;
    let stmt = stmts.into_iter().next().ok_or("empty type")?;
    match parse_statement(&stmt).map_err(|e| e.message)? {
        Ddl::CreateTable(def) => def.columns.into_iter().next().map(|c| c.ty).ok_or_else(|| "no column".into()),
        _ => Err(format!("cannot parse type '{s}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{split, Source};

    fn parse(sql: &str) -> Vec<Ddl> {
        let src = Source::new("test.sql", sql.to_string());
        split(&src, &DIALECT).unwrap().iter().map(|s| parse_statement(s).unwrap_or_else(|e| panic!("{e}"))).collect()
    }

    #[test]
    fn create_table_with_clickhouse_clauses() {
        let ddl = parse(
            "CREATE TABLE IF NOT EXISTS db.t (
                id Int32,
                ts DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),
                d DateTime('Europe/Rome'),
                n Nullable(String) COMMENT 'x; y' CODEC(ZSTD(1)),
                lc LowCardinality(String) DEFAULT 'open',
                m UInt8 MATERIALIZED id % 2,
                e String EPHEMERAL,
                _version UInt64 DEFAULT toUnixTimestamp64Nano(now64(9)),
                _deleted UInt8 DEFAULT 0,
                INDEX idx id TYPE bloom_filter GRANULARITY 4,
                CONSTRAINT ck_a CHECK (n IS NOT NULL) + (id > 0) <= 1,
                CONSTRAINT ck_b CHECK id >= 1
            )
            ENGINE = ReplacingMergeTree(_version, _deleted)
            PARTITION BY toYYYYMM(ts)
            ORDER BY (id, ts)
            SETTINGS allow_nullable_key = 1
            COMMENT 'Grain: one row';",
        );
        let Ddl::CreateTable(def) = &ddl[0] else { panic!("{ddl:?}") };
        assert_eq!(def.name, "t");
        assert!(def.if_not_exists);
        let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "ts", "d", "n", "lc", "m", "e", "_version", "_deleted"]);
        assert_eq!(def.columns[1].ty, DataType::DateTime { precision: Some(6), tz: Some("UTC".into()) });
        assert_eq!(def.columns[2].ty, DataType::DateTime { precision: None, tz: Some("Europe/Rome".into()) });
        assert_eq!(def.columns[3].comment.as_deref(), Some("x; y"));
        assert!(def.columns[3].ty.is_nullable());
        assert_eq!(def.columns[5].default.as_ref().unwrap().kind, DefaultKind::Materialized);
        assert_eq!(def.columns[6].default.as_ref().unwrap().kind, DefaultKind::Ephemeral);
        assert_eq!(def.checks.len(), 2);
        assert_eq!(def.checks[0].expr.sql, "(n IS NOT NULL) + (id > 0) <= 1");
        assert_eq!(def.engine.as_ref().unwrap().name, "ReplacingMergeTree");
        assert_eq!(def.order_by.len(), 2);
        assert_eq!(def.partition_by.as_ref().unwrap().sql, "toYYYYMM(ts)");
        assert_eq!(def.comment.as_deref(), Some("Grain: one row"));
        let sem = table_semantics(def.engine.as_ref(), &def.order_by);
        assert!(sem.collapsing);
        assert_eq!(sem.fixed, vec![("_deleted".to_string(), 0)]);
        assert_eq!(sem.keys[0].parts, vec![KeyPart::Column("id".into()), KeyPart::Column("ts".into())]);
    }

    #[test]
    fn alter_drop_rename_insert() {
        let ddl = parse(
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS r LowCardinality(String) DEFAULT 'x' COMMENT 'c' AFTER id,
                           MODIFY COLUMN ts DateTime64(3) CODEC(ZSTD(1)),
                           MODIFY COLUMN n COMMENT 'new',
                           ADD INDEX IF NOT EXISTS i n TYPE minmax GRANULARITY 1,
                           DROP COLUMN IF EXISTS old,
                           RENAME COLUMN a TO b;
             ALTER TABLE t MATERIALIZE INDEX i;
             ALTER TABLE t MODIFY SETTING min_age_to_force_merge_seconds = 259200;
             DROP TABLE IF EXISTS a, db.b;
             RENAME TABLE x TO y, p TO q;
             INSERT INTO t (a) VALUES ('a;b');
             INSERT INTO translation SELECT * FROM values('a String', ('x')) WHERE 1 NOT IN (SELECT 1 FROM translation FINAL);
             CREATE MATERIALIZED VIEW mv TO target AS SELECT * FROM t",
        );
        let Ddl::AlterTable { table, actions } = &ddl[0] else { panic!() };
        assert_eq!(table, "t");
        assert!(matches!(&actions[0], AlterAction::AddColumn { column, if_not_exists: true, after: Some(ColumnPosition::After(a)) } if column.name == "r" && a == "id"));
        assert!(matches!(&actions[1], AlterAction::ModifyColumn { ty: Some(DataType::DateTime { precision: Some(3), .. }), .. }));
        assert!(matches!(&actions[2], AlterAction::ModifyColumn { ty: None, comment: Some(c), .. } if c == "new"));
        assert!(matches!(&actions[3], AlterAction::Ignored(_)));
        assert!(matches!(&actions[4], AlterAction::DropColumn { if_exists: true, .. }));
        assert!(matches!(&actions[5], AlterAction::RenameColumn { .. }));
        assert!(matches!(&ddl[1], Ddl::AlterTable { actions, .. } if matches!(actions[0], AlterAction::Ignored(_))));
        assert!(matches!(&ddl[3], Ddl::DropTables { names, if_exists: true } if names == &["a", "b"]));
        assert!(matches!(&ddl[4], Ddl::RenameTables(p) if p.len() == 2));
        assert!(matches!(&ddl[5], Ddl::Insert { table } if table == "t"));
        assert!(matches!(&ddl[6], Ddl::Insert { table } if table == "translation"));
        assert!(matches!(&ddl[7], Ddl::MaterializedView { target: Some(t), .. } if t == "target"));
    }

    #[test]
    fn types_from_the_catalog() {
        assert_eq!(parse_type("Nullable(DateTime64(6, 'UTC'))").unwrap(), DataType::Nullable(Box::new(DataType::DateTime { precision: Some(6), tz: Some("UTC".into()) })));
        assert_eq!(parse_type("Decimal(4, 3)").unwrap(), DataType::Decimal { precision: 4, scale: 3 });
        assert_eq!(parse_type("Decimal32(2)").unwrap(), DataType::Decimal { precision: 9, scale: 2 });
        assert_eq!(parse_type("Enum8('a' = 1, 'b' = -2)").unwrap(), DataType::Enum { bits: 8, variants: vec![("a".into(), 1), ("b".into(), -2)] });
        assert_eq!(parse_type("LowCardinality(Nullable(String))").unwrap().base(), &DataType::String { max_len: None });
        assert_eq!(parse_type("Map(String, Array(UInt8))").unwrap().to_string(), "Map(String, Array(UInt8))");
        assert_eq!(parse_type("Int8").unwrap(), DataType::Int { bits: 8, signed: true });
        assert_eq!(parse_type("IPv6").unwrap(), DataType::Ipv6);
        assert_eq!(parse_type("DateTime").unwrap(), DataType::DateTime { precision: None, tz: None });
        assert!(matches!(parse_type("AggregateFunction(uniq, String)").unwrap(), DataType::Unsupported(_)));
    }

    #[test]
    fn errors_point_at_the_source() {
        let src = Source::new("m.sql", "CREATE TABLE t (a Int32)\nENGINE = MergeTree ORDER BY a GARBAGE".into());
        let stmts = split(&src, &DIALECT).unwrap();
        // GARBAGE is not a clause: it extends the ORDER BY expression and fails there.
        let e = parse_statement(&stmts[0]).unwrap_err();
        assert!(e.to_string().starts_with("m.sql:2:"), "{e}");
    }
}
