//! Database backends. Each one knows how to reach its server, read its catalog, encode
//! rows in its bulk format and phrase the verification queries. Adding a database means
//! adding a variant here and a module next to `clickhouse.rs`.

pub mod clickhouse;

use std::collections::HashMap;
use std::fmt;

use crate::config::Config;
use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::schema::{DataType, Table};
use crate::value::Value;

/// A failed database call.
#[derive(Debug, Clone)]
pub struct DbError {
    pub message: String,
    /// Worth retrying: nothing was written, or the server was momentarily unavailable.
    pub transient: bool,
    /// The server could not be reached at all.
    pub connect: bool,
}

impl DbError {
    pub fn transient(message: String) -> DbError {
        DbError { message, transient: true, connect: false }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl DbError {
    pub fn into_error(self, context: &str) -> Error {
        if self.connect {
            Error::Connect(format!("{context}: {}", self.message))
        } else {
            Error::Db(format!("{context}: {}", self.message))
        }
    }
}

#[derive(Debug, Clone)]
pub enum Database {
    Clickhouse(clickhouse::Client),
}

impl Database {
    pub fn connect(config: &Config) -> Result<Database> {
        let url = config.database_url()?;
        match config.dialect {
            Dialect::Clickhouse => Ok(Database::Clickhouse(clickhouse::Client::connect(url, config.generation.http_timeout_secs)?)),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Database::Clickhouse(c) => c.describe(),
        }
    }

    /// A file-name-safe identity of the target, for the resume state.
    pub fn state_key(&self) -> String {
        match self {
            Database::Clickhouse(c) => {
                let raw = format!("{}_{}_{}", c.host, c.port, c.database);
                raw.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' }).collect()
            }
        }
    }

    pub fn version(&self) -> Result<String> {
        match self {
            Database::Clickhouse(c) => c.version().map_err(|e| e.into_error("server version")),
        }
    }

    pub fn live_columns(&self) -> Result<HashMap<String, Vec<(String, String)>>> {
        match self {
            Database::Clickhouse(c) => c.live_columns().map_err(|e| e.into_error("reading the catalog")),
        }
    }

    pub fn parse_type(&self, s: &str) -> std::result::Result<DataType, String> {
        match self {
            Database::Clickhouse(_) => Dialect::Clickhouse.parse_type(s),
        }
    }

    pub fn count(&self, table: &str) -> std::result::Result<u64, DbError> {
        match self {
            Database::Clickhouse(c) => c.count(table),
        }
    }

    pub fn truncate(&self, table: &str) -> std::result::Result<(), DbError> {
        match self {
            Database::Clickhouse(c) => c.truncate(table),
        }
    }

    pub fn execute(&self, sql: &str) -> std::result::Result<(), DbError> {
        match self {
            Database::Clickhouse(c) => c.execute(sql),
        }
    }

    pub fn query_u64(&self, sql: &str) -> std::result::Result<u64, DbError> {
        match self {
            Database::Clickhouse(c) => c.query_u64(sql),
        }
    }

    /// Appends one row in the backend's bulk format.
    pub fn encode(&self, buf: &mut Vec<u8>, ty: &DataType, v: &Value) -> std::result::Result<(), String> {
        match self {
            Database::Clickhouse(_) => clickhouse::encode(buf, ty, v),
        }
    }

    pub fn insert(&self, table: &Table, columns: &[&str], body: &[u8]) -> std::result::Result<(), DbError> {
        match self {
            Database::Clickhouse(c) => {
                let json = table.columns.iter().any(|col| col.ty.contains_json() && columns.contains(&col.name.as_str()));
                c.insert(&table.name, columns, body, json)
            }
        }
    }

    pub fn duplicates_sql(&self, table: &Table, key: &[String]) -> String {
        match self {
            Database::Clickhouse(c) => c.duplicates_sql(table, key),
        }
    }

    pub fn collapsed_sql(&self, table: &Table) -> Option<String> {
        match self {
            Database::Clickhouse(c) => table.collapsing.then(|| c.collapsed_sql(table)),
        }
    }

    pub fn orphans_sql(&self, child: &Table, child_cols: &[&str], parent: &Table, parent_cols: &[&str]) -> String {
        match self {
            Database::Clickhouse(c) => c.orphans_sql(child, child_cols, parent, parent_cols),
        }
    }

    pub fn violations_sql(&self, table: &Table, check_sql: &str) -> String {
        match self {
            Database::Clickhouse(c) => c.violations_sql(table, check_sql),
        }
    }

    pub fn quote(&self, ident: &str) -> String {
        match self {
            Database::Clickhouse(_) => clickhouse::Client::quote(ident),
        }
    }
}

impl crate::plan::RowSource for Database {
    fn read_rows(&self, table: &Table, columns: &[usize], limit: u64) -> Result<Vec<Vec<Value>>> {
        match self {
            Database::Clickhouse(c) => c.read_rows(table, columns, limit).map_err(|e| e.into_error(&format!("reading {}", table.name))),
        }
    }
}
