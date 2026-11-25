//! ClickHouse, over its HTTP interface.
//!
//! - DDL: [`dialect`], a token-level adapter, because `sqlparser` parses ClickHouse types,
//!   columns and expressions but not the structure of its `CREATE TABLE` and
//!   `ALTER TABLE` statements. It also knows what table engines imply: sorting keys that
//!   merge rows, columns that must hold a fixed value, tables that store no rows.
//! - Loading: [`backend`], `INSERT ... FORMAT RowBinary`, one HTTP request per batch,
//!   encoded from the column types of the compiled schema. The binary format depends on
//!   the exact column types, so the live types must match the DDL.
//! - Foreign keys are not enforced, so tables load in any order.

pub mod backend;
pub mod dialect;

use sqlparser::dialect::Dialect as SqlDialect;

pub use backend::Client;

use crate::config::Config;
use crate::connector::{Connector, Dialect, TableSemantics};
use crate::db::Backend;
use crate::error::Result;
use crate::schema::DataType;
use crate::sql::convert::TypeRules;
use crate::sql::{Ddl, Diag, EngineSpec, ExprSpec, Statement, TableDef};

/// The ClickHouse connector.
pub struct ClickHouseConnector;

/// The handle of the ClickHouse connector.
pub const CONNECTOR: Dialect = Dialect::new(&ClickHouseConnector);

#[allow(non_upper_case_globals)]
impl Dialect {
    /// ClickHouse, as it was named when dialects were an enum.
    pub const Clickhouse: Dialect = CONNECTOR;
}

impl Connector for ClickHouseConnector {
    fn name(&self) -> &'static str {
        "clickhouse"
    }

    fn product(&self) -> &'static str {
        "ClickHouse"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ch"]
    }

    fn schemes(&self) -> &'static [&'static str] {
        &["clickhouse", "clickhouses", "ch", "http", "https"]
    }

    fn connect(&self, url: &str, config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Client::connect(url, config.generation.http_timeout_secs)?))
    }

    fn sql(&self) -> &'static dyn SqlDialect {
        &dialect::DIALECT
    }

    fn type_rules(&self) -> TypeRules {
        dialect::RULES
    }

    fn parse_statement(&self, stmt: &Statement) -> std::result::Result<Ddl, Diag> {
        dialect::parse_statement(stmt)
    }

    fn parse_type(&self, s: &str) -> std::result::Result<DataType, String> {
        dialect::parse_type(s)
    }

    fn quote_ident(&self, name: &str) -> String {
        Client::quote(name)
    }

    fn table_semantics(&self, engine: Option<&EngineSpec>, order_by: &[ExprSpec]) -> TableSemantics {
        dialect::table_semantics(engine, order_by)
    }

    fn is_storage_table(&self, def: &TableDef) -> bool {
        dialect::is_storage_table(def)
    }

    fn enforces_foreign_keys(&self) -> bool {
        false
    }
}
