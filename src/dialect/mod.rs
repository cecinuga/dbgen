//! SQL dialects and the database semantics that come with them.
//!
//! A dialect knows how to turn one statement of its SQL into dialect-neutral [`Ddl`]
//! commands (with `sqlparser` doing the parsing), which spellings mean which types, how
//! to quote identifiers, and which table definitions imply uniqueness. Adding a database
//! means adding a variant here, a module like `clickhouse.rs`, and a backend in `db`.

pub mod clickhouse;

use serde::{Deserialize, Serialize};
use sqlparser::dialect::Dialect as SqlDialect;

use crate::schema::{DataType, Key};
use crate::sql::convert::TypeRules;
use crate::sql::{Ddl, Diag, Statement, TableDef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    #[serde(alias = "ch")]
    #[value(alias = "ch")]
    Clickhouse,
}

/// What a table definition implies beyond its columns.
#[derive(Debug, Default)]
pub struct TableSemantics {
    /// Keys whose values must be unique for rows to survive (beyond PRIMARY KEY/UNIQUE).
    pub keys: Vec<Key>,
    /// Columns the engine requires to hold a fixed value (e.g. ClickHouse's `is_deleted`
    /// of `ReplacingMergeTree` must be 0, or the row reads as deleted).
    pub fixed: Vec<(String, i64)>,
    /// Rows with equal sorting keys are merged, so reads must deduplicate (`FINAL`).
    pub collapsing: bool,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Clickhouse => "clickhouse",
        }
    }

    pub fn sql(self) -> &'static dyn SqlDialect {
        match self {
            Dialect::Clickhouse => &clickhouse::DIALECT,
        }
    }

    pub fn type_rules(self) -> TypeRules {
        match self {
            Dialect::Clickhouse => clickhouse::RULES,
        }
    }

    pub fn parse_statement(self, stmt: &Statement) -> Result<Ddl, Diag> {
        match self {
            Dialect::Clickhouse => clickhouse::parse_statement(stmt),
        }
    }

    pub fn table_semantics(self, engine: Option<&crate::sql::EngineSpec>, order_by: &[crate::sql::ExprSpec]) -> TableSemantics {
        match self {
            Dialect::Clickhouse => clickhouse::table_semantics(engine, order_by),
        }
    }

    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Dialect::Clickhouse => format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`")),
        }
    }

    /// Parses a type as the database spells it (e.g. a type from its catalog).
    pub fn parse_type(self, s: &str) -> Result<DataType, String> {
        match self {
            Dialect::Clickhouse => clickhouse::parse_type(s),
        }
    }

    /// Whether `def` creates a table that can hold generated rows.
    pub fn is_storage_table(self, def: &TableDef) -> bool {
        match self {
            Dialect::Clickhouse => clickhouse::is_storage_table(def),
        }
    }
}

/// Parses a bare type name, for types nested in modifiers (`SimpleAggregateFunction`).
pub fn parse_type_name(name: &str, rules: &TypeRules) -> Option<DataType> {
    let mut parser = sqlparser::parser::Parser::new(&clickhouse::DIALECT).try_with_sql(name).ok()?;
    let t = parser.parse_data_type().ok()?;
    Some(crate::sql::convert::data_type(&t, rules))
}
