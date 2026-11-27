//! A template for a new connector: a typical SQL database reached through a driver whose
//! client holds one connection, loaded with multi-row `INSERT` statements.
//!
//! Copy this file to `src/connectors/<name>/mod.rs`, rename `Example*`, replace the
//! `Driver` stand-in with your driver crate, and follow the comments. Then add one line to
//! the `connectors!` list in `src/connectors/mod.rs` and a feature to `Cargo.toml`. The
//! guide (`docs/connectors/tutorial.md`) walks through it.
//!
//! This file is compiled with the tests, so it always matches the API, but it is not
//! registered: dbgen never uses it.
// The stand-in driver is never constructed.
#![allow(dead_code)]

use sqlparser::dialect::{Dialect as SqlDialect, GenericDialect};

use crate::config::Config;
use crate::connector::{Connector, Dialect};
use crate::db::pool::Pool;
use crate::db::text::{TextStyle, insert_statements, sql_row};
use crate::db::{Backend, Batch, CellError, DbError, DbErrorKind, DbResult, LiveColumns, RowRef, TextRows, file_safe, group_columns};
use crate::error::{Error, Result};
use crate::schema::Table;
use crate::sql::convert::TypeRules;

// ---------------------------------------------------------------------------------------
// The connector: names, dialect, capabilities. No connection.
// ---------------------------------------------------------------------------------------

/// The connector. A unit struct: everything it knows is static.
pub struct ExampleConnector;

/// Its handle, which the registry lists (`"example" => example` in `connectors!`).
pub const CONNECTOR: Dialect = Dialect::new(&ExampleConnector);

/// The `sqlparser` dialect closest to your SQL. `GenericDialect` accepts most syntax;
/// prefer the database's own (`PostgreSqlDialect`, `MySqlDialect`, `MsSqlDialect`...).
static SQL: GenericDialect = GenericDialect {};

/// How your DDL spells types. Start from standard SQL and change what differs: here,
/// `TIMESTAMP` without a precision holds whole seconds. Type names `sqlparser` does not
/// know go in `custom_type` (see the PostgreSQL connector for an example).
const RULES: TypeRules = TypeRules { timestamp_precision: 0, ..TypeRules::STANDARD };

impl Connector for ExampleConnector {
    /// Lowercase and unique: the `dialect:` key, `--dialect`, and fixture file names
    /// (`tests/fixtures/<domain>/example.sql`).
    fn name(&self) -> &'static str {
        "example"
    }

    fn product(&self) -> &'static str {
        "ExampleDB"
    }

    /// The URL schemes that select this connector: `example://user:pw@host/db`.
    fn schemes(&self) -> &'static [&'static str] {
        &["example"]
    }

    /// Parse the URL here and fail only if it is malformed; connect lazily (the pool does).
    fn connect(&self, url: &str, _config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Example::connect(url)?))
    }

    fn sql(&self) -> &'static dyn SqlDialect {
        &SQL
    }

    fn type_rules(&self) -> TypeRules {
        RULES
    }

    // The defaults of `split`, `parse_statement` and `parse_type` use the shared adapter
    // (`crate::dialect::standard`) with `sql()` and `type_rules()`. `quote_ident` is ANSI
    // ("name"); override it if your database quotes otherwise:
    //
    // fn quote_ident(&self, name: &str) -> String {
    //     format!("`{}`", name.replace('`', "``"))
    // }
    //
    // Override `enforces_foreign_keys` with `false` if inserts do not check them.
}

// ---------------------------------------------------------------------------------------
// The backend: one target database, shared by every writer thread.
// ---------------------------------------------------------------------------------------

/// Statements stay below the server's limit on statement size.
const MAX_STATEMENT_BYTES: usize = 1 << 20;

/// How literals are spelled in `INSERT` statements.
const STYLE: TextStyle = TextStyle { true_text: "TRUE", false_text: "FALSE", backslash_escapes: false };

/// A database, reached through a pool of driver connections.
pub struct Example {
    pool: Pool<Driver>,
    host: String,
    database: String,
}

impl Example {
    /// Parses the URL; opens nothing.
    pub fn connect(url: &str) -> Result<Example> {
        let rest = url.strip_prefix("example://").ok_or_else(|| Error::Config(format!("database URL: '{url}' is not example://host/database")))?;
        let (host, database) = rest.split_once('/').ok_or_else(|| Error::Config("database URL: no database name".into()))?;
        let (host, database) = (host.to_string(), database.to_string());
        let (h, d) = (host.clone(), database.clone());
        // Each new connection sets the session up: UTC timestamps, the escaping `STYLE`
        // assumes, foreign key checks on.
        let pool = Pool::new(move || Driver::open(&h, &d).map_err(classify));
        Ok(Example { pool, host, database })
    }
}

/// Maps a driver error onto what core should do about it. This decides retries and the
/// `--create-database` hint, so classify by the server's error codes, not by message text.
fn classify(e: DriverError) -> DbError {
    let kind = match e.code {
        Some(1) => DbErrorKind::UnknownDatabase, // "database does not exist"
        Some(2) => DbErrorKind::Transient,       // deadlock, lock timeout, too many connections
        None => DbErrorKind::Unreachable,        // no answer from the server
        _ => DbErrorKind::Failed,                // everything deterministic
    };
    DbError::new(kind, format!("{} (error {:?})", e.message, e.code))
}

impl Backend for Example {
    fn dialect(&self) -> Dialect {
        CONNECTOR
    }

    fn describe(&self) -> String {
        format!("example {}/{}", self.host, self.database)
    }

    fn state_key(&self) -> String {
        file_safe(&format!("{}_{}", self.host, self.database))
    }

    fn version(&self) -> DbResult<String> {
        let rows = self.query_rows("SELECT version()")?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).flatten().unwrap_or_default())
    }

    fn create_database(&self) -> DbResult<()> {
        // Connect without a database (or to an administrative one) and create it.
        let mut admin = Driver::open(&self.host, "").map_err(classify)?;
        admin.execute(&format!("CREATE DATABASE IF NOT EXISTS {}", self.quote(&self.database))).map_err(classify)
    }

    fn execute(&self, sql: &str) -> DbResult<()> {
        self.pool.with(|c| c.execute(sql).map_err(classify))
    }

    fn query_rows(&self, sql: &str) -> DbResult<TextRows> {
        self.pool.with(|c| c.query(sql).map_err(classify))
    }

    fn live_columns(&self) -> DbResult<LiveColumns> {
        // Every column, generated ones included, in table order.
        let rows = self.query_rows(
            "SELECT table_name, column_name, data_type FROM information_schema.columns
             WHERE table_schema = current_schema() ORDER BY table_name, ordinal_position",
        )?;
        Ok(group_columns(rows))
    }

    fn encode_row(&self, out: &mut Vec<u8>, row: &RowRef<'_>) -> std::result::Result<(), CellError> {
        sql_row(out, row, STYLE)
    }

    fn insert(&self, table: &Table, columns: &[&str], batch: &Batch) -> DbResult<()> {
        let cols: Vec<String> = columns.iter().map(|c| self.quote(c)).collect();
        let prefix = format!("INSERT INTO {} ({}) VALUES ", self.table_ref(&table.name), cols.join(", "));
        let statements = insert_statements(&prefix, batch, MAX_STATEMENT_BYTES);
        // All statements of a batch in one transaction: the batch is written whole or not at all.
        self.pool.with(|c| {
            c.execute("BEGIN").map_err(classify)?;
            for sql in &statements {
                if let Err(e) = c.execute(sql) {
                    let _ = c.execute("ROLLBACK");
                    return Err(classify(e));
                }
            }
            c.execute("COMMIT").map_err(classify)
        })
    }

    fn truncate(&self, tables: &[&str]) -> DbResult<()> {
        // Foreign keys must not get in the way: truncate them together, or turn checks off.
        let list: Vec<String> = tables.iter().map(|t| self.table_ref(t)).collect();
        if list.is_empty() {
            return Ok(());
        }
        self.execute(&format!("TRUNCATE TABLE {}", list.join(", ")))
    }

    // Optional overrides, with their defaults:
    // - finish_load: nothing. Advance identity sequences and refresh statistics here.
    // - max_writers: no limit. 1 for a database with a single writer.
    // - strict_types: false. true if the bulk format is binary and needs the exact types.
    // - table_ref: the quoted name. Qualify it if queries need `database.table`.
    // - cast_to_text: `CAST(x AS TEXT)`, used by read_rows.
    // - count, read_rows, duplicates_sql, orphans_sql, violations_sql: portable SQL.
    // - introspect_ddl: an error saying the connector cannot read the schema. Return the
    //   database's DDL (docs/connectors/reference.md#schema-introspection) to let dbgen
    //   run without DDL files.
}

// ---------------------------------------------------------------------------------------
// A stand-in for your driver crate, so this file compiles. Delete it.
// ---------------------------------------------------------------------------------------

struct Driver;

struct DriverError {
    code: Option<u32>,
    message: String,
}

impl Driver {
    fn open(_host: &str, _database: &str) -> std::result::Result<Driver, DriverError> {
        Err(DriverError { code: None, message: "the template has no driver".into() })
    }

    fn execute(&mut self, _sql: &str) -> std::result::Result<(), DriverError> {
        unreachable!("never opened")
    }

    fn query(&mut self, _sql: &str) -> std::result::Result<TextRows, DriverError> {
        unreachable!("never opened")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::DataType;

    /// The DDL side works before any driver code: keep a test like this one.
    #[test]
    fn parses_its_types() {
        assert_eq!(CONNECTOR.parse_type("INTEGER").unwrap(), DataType::Int { bits: 32, signed: true });
        assert_eq!(CONNECTOR.parse_type("TIMESTAMP").unwrap(), DataType::DateTime { precision: Some(0), tz: None });
        assert!(Example::connect("example://localhost/shop").is_ok());
        assert!(Example::connect("other://localhost/shop").is_err());
    }
}
