//! Checks the loaded data from the database side: exact row counts, unique keys, no rows
//! lost to collapsing merges, references that resolve, CHECK constraints that hold.

use std::fmt::Write as _;

use serde::Serialize;

use crate::db::Database;
use crate::plan::Plan;
use crate::schema::{KeyPart, Schema};

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    Pass,
    Fail,
    Skip,
    Error,
}

/// One verification check and what it found.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub violations: Option<u64>,
    pub detail: String,
}

fn counted(name: String, sql: &str, db: &Database, what: &str) -> CheckResult {
    match db.query_u64(sql) {
        Ok(0) => CheckResult { name, status: CheckStatus::Pass, violations: Some(0), detail: String::new() },
        Ok(n) => CheckResult { name, status: CheckStatus::Fail, violations: Some(n), detail: format!("{n} {what}") },
        Err(e) => CheckResult { name, status: CheckStatus::Error, violations: None, detail: e.message },
    }
}

/// Runs every check on the generated tables. Checks on empty tables are skipped.
pub fn run(db: &Database, schema: &Schema, plan: &Plan) -> Vec<CheckResult> {
    let mut out = Vec::new();
    let mut counts = std::collections::HashMap::new();
    for (ti, tg) in plan.generated() {
        let t = &schema.tables[ti];
        let count = match db.count(&t.name) {
            Ok(n) => n,
            Err(e) => {
                out.push(CheckResult { name: format!("rows.{}", t.name), status: CheckStatus::Error, violations: None, detail: e.message });
                continue;
            }
        };
        counts.insert(ti, count);
        out.push(if count == tg.rows {
            CheckResult { name: format!("rows.{}", t.name), status: CheckStatus::Pass, violations: Some(0), detail: format!("{count} rows") }
        } else {
            CheckResult {
                name: format!("rows.{}", t.name),
                status: CheckStatus::Fail,
                violations: Some(count.abs_diff(tg.rows)),
                detail: format!("{count} rows, expected {}", tg.rows),
            }
        });
        if count == 0 {
            continue;
        }
        for key in &t.keys {
            let parts: Vec<String> = key
                .parts
                .iter()
                .map(|p| match p {
                    KeyPart::Column(c) => db.quote(c),
                    KeyPart::Expr(s) => s.clone(),
                })
                .collect();
            let name = format!("unique.{}.({})", t.name, key.parts.iter().map(|p| match p {
                KeyPart::Column(c) | KeyPart::Expr(c) => c.as_str(),
            }).collect::<Vec<_>>().join(", "));
            out.push(counted(name, &db.duplicates_sql(t, &parts), db, "duplicate rows"));
        }
        if let Some(sql) = db.collapsed_sql(t) {
            out.push(counted(format!("collapse.{}", t.name), &sql, db, "rows merged away"));
        }
        for ch in &t.checks {
            out.push(counted(format!("check.{}.{}", t.name, ch.name), &db.violations_sql(t, &ch.sql), db, "rows violate it"));
        }
    }
    for r in &plan.relations {
        let (Some(ci), Some(pi)) = (schema.table_index(&r.child), schema.table_index(&r.parent)) else { continue };
        let name = format!("reference.{}.{} -> {}.{}", r.child, r.child_cols.join(","), r.parent, r.parent_cols.join(","));
        if counts.get(&ci) == Some(&0) {
            out.push(CheckResult { name, status: CheckStatus::Skip, violations: None, detail: "no rows".into() });
            continue;
        }
        let cc: Vec<&str> = r.child_cols.iter().map(String::as_str).collect();
        let pc: Vec<&str> = r.parent_cols.iter().map(String::as_str).collect();
        out.push(counted(name, &db.orphans_sql(&schema.tables[ci], &cc, &schema.tables[pi], &pc), db, "references without a parent row"));
    }
    out
}

/// The results as text, with totals.
pub fn render(results: &[CheckResult]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\nVERIFICATION\n");
    for r in results {
        let _ = writeln!(s, "  {:<5} {:<80} {}", format!("{:?}", r.status).to_uppercase(), r.name, r.detail);
    }
    let count = |st: CheckStatus| results.iter().filter(|r| r.status == st).count();
    let _ = writeln!(
        s,
        "\nPassed: {}  Failed: {}  Skipped: {}  Errors: {}",
        count(CheckStatus::Pass),
        count(CheckStatus::Fail),
        count(CheckStatus::Skip),
        count(CheckStatus::Error)
    );
    s
}

/// Whether no check failed or errored.
pub fn all_passed(results: &[CheckResult]) -> bool {
    results.iter().all(|r| matches!(r.status, CheckStatus::Pass | CheckStatus::Skip))
}
