//! SQLite, through the bundled library: no server, a database is a file.
//!
//! - DDL: the shared adapter ([`crate::dialect::standard`]) with `sqlparser`'s SQLite
//!   dialect. Columns may omit their type; they hold text.
//! - Loading: [`backend`], multi-row `INSERT` statements in one transaction per batch, one
//!   writer at a time. dbgen turns foreign key checks on, which SQLite leaves off.

pub mod backend;

use sqlparser::dialect::{Dialect as SqlDialect, SQLiteDialect};

pub use backend::Sqlite;

use crate::config::Config;
use crate::connector::{Connector, Dialect};
use crate::db::Backend;
use crate::error::Result;
use crate::sql::convert::{self, TypeRules};

/// The SQLite connector.
pub struct SqliteConnector;

/// The handle of the SQLite connector.
pub const CONNECTOR: Dialect = Dialect::new(&SqliteConnector);

#[allow(non_upper_case_globals)]
impl Dialect {
    /// SQLite, as it was named when dialects were an enum.
    pub const Sqlite: Dialect = CONNECTOR;
}

static SQL: SQLiteDialect = SQLiteDialect {};

/// Standard SQL rules; `DATETIME` without a precision holds whole seconds. SQLite accepts
/// any type name, so it keeps the legacy set of custom names every dialect accepted before
/// connectors (see [`convert::compat_custom_type`]).
pub const RULES: TypeRules = TypeRules {
    timestamp_precision: 0,
    custom_type: convert::compat_custom_type,
    identity_types: convert::COMPAT_IDENTITY_TYPES,
    ..TypeRules::STANDARD
};

impl Connector for SqliteConnector {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn product(&self) -> &'static str {
        "SQLite"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["sqlite3"]
    }

    fn schemes(&self) -> &'static [&'static str] {
        &["sqlite", "sqlite3", "file"]
    }

    /// A relative file path is resolved against the configuration's directory.
    fn connect(&self, url: &str, config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Sqlite::connect(url, &config.base_dir)?))
    }

    fn sql(&self) -> &'static dyn SqlDialect {
        &SQL
    }

    fn type_rules(&self) -> TypeRules {
        RULES
    }
}
