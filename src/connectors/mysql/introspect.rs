//! The DDL of a live MySQL database, for
//! [`Backend::introspect_ddl`](crate::db::Backend::introspect_ddl): `SHOW CREATE TABLE` of
//! every base table, parents first, made to read back as it was declared.
//!
//! `SHOW CREATE TABLE` normalizes what it prints. Three of its spellings compile to
//! something else than the declaration they come from, and are written back:
//!
//! - `BOOLEAN` columns print as `tinyint(1)`: written `boolean` again. (A column declared
//!   `TINYINT(1)` reads back as a boolean too; MySQL keeps no difference.)
//! - Nullable columns without a default print `DEFAULT NULL`, which dbgen would read as a
//!   database default and leave to the server: dropped, which means the same in MySQL.
//! - String literals in CHECKs and expressions print with a character set introducer
//!   (`_utf8mb4'web'`), which dbgen does not evaluate: dropped.

use crate::db::{DbError, DbResult, TextRows};

/// The DDL of the current database, reading it with `query` and quoting table names with
/// `table_ref`.
pub(super) fn ddl(query: impl Fn(&str) -> DbResult<TextRows>, table_ref: impl Fn(&str) -> String) -> DbResult<String> {
    let first = |rows: TextRows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect::<Vec<String>>();
    let tables = first(query("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME")?);
    let references: Vec<(String, String)> = query("SELECT TABLE_NAME, REFERENCED_TABLE_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE()")?
        .into_iter()
        .filter_map(|r| match r.as_slice() {
            [Some(child), Some(parent)] => Some((child.clone(), parent.clone())),
            _ => None,
        })
        .collect();
    let mut out = String::new();
    for table in parents_first(tables, &references) {
        let rows = query(&format!("SHOW CREATE TABLE {}", table_ref(&table)))?;
        let ddl = rows.into_iter().next().and_then(|r| r.into_iter().nth(1).flatten());
        let ddl = ddl.ok_or_else(|| DbError::failed(format!("SHOW CREATE TABLE {table} returned nothing")))?;
        out.push_str(&declared(&ddl));
        out.push_str(";\n\n");
    }
    Ok(out)
}

/// Orders `tables` so every table comes after the tables it references (`(child,
/// parent)` pairs), so the script also applies as it is; alphabetically otherwise. Tables
/// in a reference cycle come last, alphabetically.
fn parents_first(mut tables: Vec<String>, references: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tables.len());
    while !tables.is_empty() {
        let ready = tables.iter().position(|t| references.iter().all(|(c, p)| c != t || p == t || !tables.contains(p)));
        match ready {
            Some(i) => out.push(tables.remove(i)),
            None => out.append(&mut tables),
        }
    }
    out
}

/// A `SHOW CREATE TABLE` statement in the spelling it was declared with (see the module
/// documentation).
fn declared(ddl: &str) -> String {
    let lines: Vec<String> = strip_introducers(ddl)
        .lines()
        .map(|line| {
            if !line.starts_with("  `") {
                return line.to_string();
            }
            let line = line.replacen("` tinyint(1) ", "` boolean ", 1).replacen("` tinyint(1),", "` boolean,", 1);
            let (body, comma) = match line.strip_suffix(',') {
                Some(body) => (body, ","),
                None => (line.as_str(), ""),
            };
            match body.strip_suffix(" DEFAULT NULL") {
                Some(rest) => format!("{rest}{comma}"),
                None => line.replacen(" DEFAULT NULL COMMENT ", " COMMENT ", 1),
            }
        })
        .collect();
    lines.join("\n")
}

/// Drops the character set introducers before string literals (`_utf8mb4'x'` is `'x'`),
/// leaving quoted strings and identifiers alone.
fn strip_introducers(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' if q != b'`' => i += 2,
                        c if c == q && b.get(i + 1) == Some(&q) => i += 2,
                        c if c == q => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'_' if i == 0 || !(b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b'$')) => {
                let name_end = i + 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric()).count();
                if name_end > i + 1 && b.get(name_end) == Some(&b'\'') {
                    out.push_str(&sql[copied..i]);
                    copied = name_end;
                }
                i = name_end;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[copied.min(sql.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_read_as_declared() {
        let shown = "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `ok` tinyint(1) NOT NULL,\n  `note` varchar(9) DEFAULT NULL,\n  \
                     `nick` text DEFAULT NULL COMMENT 'x',\n  `kind` varchar(9) DEFAULT 'NULL',\n  PRIMARY KEY (`id`),\n  \
                     CONSTRAINT `ck` CHECK ((`kind` in (_utf8mb4'a',_latin1'it''s _x''y'))),\n  CONSTRAINT `ok_2` CHECK ((`ok` = 1))\n) ENGINE=InnoDB";
        let expected = "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `ok` boolean NOT NULL,\n  `note` varchar(9),\n  \
                        `nick` text COMMENT 'x',\n  `kind` varchar(9) DEFAULT 'NULL',\n  PRIMARY KEY (`id`),\n  \
                        CONSTRAINT `ck` CHECK ((`kind` in ('a','it''s _x''y'))),\n  CONSTRAINT `ok_2` CHECK ((`ok` = 1))\n) ENGINE=InnoDB";
        assert_eq!(declared(shown), expected);
        assert_eq!(strip_introducers("`_utf8mb4`='x' AND a_b'"), "`_utf8mb4`='x' AND a_b'");
    }

    #[test]
    fn parents_come_first() {
        let t = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let refs = [("a".to_string(), "c".to_string()), ("c".to_string(), "b".to_string()), ("b".to_string(), "b".to_string())];
        assert_eq!(parents_first(t(&["a", "b", "c", "d"]), &refs), t(&["b", "c", "a", "d"]));
        let cycle = [("x".to_string(), "y".to_string()), ("y".to_string(), "x".to_string())];
        assert_eq!(parents_first(t(&["x", "y", "z"]), &cycle), t(&["z", "x", "y"]));
    }
}
