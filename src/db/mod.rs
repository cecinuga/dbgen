//! Connected databases: the [`Backend`] trait each connector implements, the
//! [`Database`] the rest of dbgen holds, and the toolkit backends share.
//!
//! A backend knows how to reach its server, read its catalog, encode rows in its bulk
//! format and run statements; the [`Backend`] trait gives the rest (counts, verification
//! queries, reading reference rows) in portable SQL, which a backend overrides where its
//! SQL differs. The backends themselves live in [`crate::connectors`].
//!
//! Toolkit: [`pool`] (a connection pool for drivers whose clients hold one connection),
//! [`text`] (PostgreSQL's `COPY` text format, SQL literals and multi-row `INSERT`
//! statements), [`group_columns`] and [`file_safe`].

pub mod pool;
pub mod text;

/// Compatibility path of the ClickHouse backend, from before connectors:
/// [`crate::connectors::clickhouse::backend`].
#[cfg(feature = "clickhouse")]
pub use crate::connectors::clickhouse::backend as clickhouse;

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;

use crate::config::Config;
use crate::connector::Dialect;
use crate::error::{Error, Result};
use crate::expr::cast;
use crate::schema::{DataType, Table};
use crate::value::Value;

/// What went wrong in a database call, as far as the caller's reaction goes. A connector
/// classifies its driver's errors into these, by error code where the server has them;
/// core's retries, exit codes and hints depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorKind {
    /// Deterministic: retrying would fail the same way (a constraint violation, a syntax
    /// error, a value the column rejects). The load stops.
    Failed,
    /// Worth retrying: nothing was written, or the server was momentarily unable to serve
    /// (deadlock, lock timeout, too many connections, a server shutting down). A batch is
    /// retried up to `max_retries` times; [`pool::Pool`] drops the connection it failed on.
    Transient,
    /// The server could not be reached. Retried during a load like a transient failure;
    /// fatal when connecting (exit code 3).
    Unreachable,
    /// The server is up but the database does not exist: the message suggests
    /// `dbgen apply --create-database`.
    UnknownDatabase,
}

/// A failed database call.
#[derive(Debug, Clone)]
pub struct DbError {
    pub message: String,
    pub kind: DbErrorKind,
}

impl DbError {
    /// An error of kind `kind`.
    pub fn new(kind: DbErrorKind, message: impl Into<String>) -> DbError {
        DbError { message: message.into(), kind }
    }

    /// A deterministic failure: retrying would not help.
    pub fn failed(message: impl Into<String>) -> DbError {
        DbError::new(DbErrorKind::Failed, message)
    }

    /// A failure worth retrying.
    pub fn transient(message: impl Into<String>) -> DbError {
        DbError::new(DbErrorKind::Transient, message)
    }

    /// Whether the call may succeed if repeated.
    pub fn is_transient(&self) -> bool {
        matches!(self.kind, DbErrorKind::Transient | DbErrorKind::Unreachable)
    }

    /// The process-level error, with `context` saying what was being done.
    pub fn into_error(self, context: &str) -> Error {
        match self.kind {
            DbErrorKind::Unreachable => Error::Connect(format!("{context}: {}", self.message)),
            DbErrorKind::UnknownDatabase => {
                Error::Db(format!("{context}: {}\n`dbgen apply --create-database` creates it and applies the DDL", self.message))
            }
            DbErrorKind::Failed | DbErrorKind::Transient => Error::Db(format!("{context}: {}", self.message)),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DbError {}

/// The result of a database call.
pub type DbResult<T> = std::result::Result<T, DbError>;

/// Table name -> its columns as `(name, type as the server spells it)`.
pub type LiveColumns = HashMap<String, Vec<(String, String)>>;

/// Rows of query results, every value as text (`None` for NULL).
pub type TextRows = Vec<Vec<Option<String>>>;

/// One batch of rows in a backend's bulk format.
#[derive(Debug, Default)]
pub struct Batch {
    pub body: Vec<u8>,
    /// Offset in `body` where each row ends, for backends that split a batch into
    /// several statements.
    pub row_ends: Vec<usize>,
}

impl Batch {
    /// Number of rows.
    pub fn rows(&self) -> usize {
        self.row_ends.len()
    }

    /// The rows, each as its encoded bytes.
    pub fn row_slices(&self) -> impl Iterator<Item = &[u8]> {
        let starts = std::iter::once(0).chain(self.row_ends.iter().copied());
        starts.zip(self.row_ends.iter().copied()).map(|(a, b)| &self.body[a..b])
    }
}

/// The cells of one row to encode: `values[columns[k]]` goes to a column of type
/// `types[k]`, for `k` in insert order.
pub struct RowRef<'a> {
    pub types: &'a [&'a DataType],
    pub values: &'a [Value],
    pub columns: &'a [usize],
}

impl RowRef<'_> {
    /// `(position in insert order, type, value)` of every cell.
    pub fn cells(&self) -> impl Iterator<Item = (usize, &DataType, &Value)> {
        self.columns.iter().zip(self.types).enumerate().map(|(k, (&c, &ty))| (k, ty, &self.values[c]))
    }
}

/// A value that cannot be encoded for its column; `column` is its position in insert order.
#[derive(Debug)]
pub struct CellError {
    pub column: usize,
    pub message: String,
}

/// What dbgen needs from a connected database: the runtime half of a connector (see
/// [`crate::connector`]).
///
/// One value serves the whole run and is shared by every writer thread, so methods take
/// `&self` and must be safe to call concurrently (up to [`max_writers`](Backend::max_writers)
/// inserts at a time). Calls that fail return a [`DbError`] whose [`DbErrorKind`] tells core
/// what to do: retry a [`Transient`](DbErrorKind::Transient) or
/// [`Unreachable`](DbErrorKind::Unreachable) failure, stop on a
/// [`Failed`](DbErrorKind::Failed) one, suggest `--create-database` on
/// [`UnknownDatabase`](DbErrorKind::UnknownDatabase).
///
/// The first eleven methods are required; the rest have defaults in portable SQL.
pub trait Backend: Send + Sync {
    /// The handle of the connector this backend belongs to (its `CONNECTOR`).
    fn dialect(&self) -> Dialect;

    /// Where the data goes, for messages: `postgres localhost:5432/shop`. No password.
    fn describe(&self) -> String;

    /// A file-name-safe identity of the target (server and database), naming its resume
    /// state: two configurations loading the same database must get the same key, two
    /// databases different keys. [`file_safe`] makes any string safe.
    fn state_key(&self) -> String;

    /// The server version. The first call that reaches the server: when the database does
    /// not exist it fails with [`DbErrorKind::UnknownDatabase`], when the server cannot be
    /// reached with [`DbErrorKind::Unreachable`].
    fn version(&self) -> DbResult<String>;

    /// Creates the database if it does not exist (`dbgen apply --create-database`);
    /// succeeds if it does.
    fn create_database(&self) -> DbResult<()>;

    /// Runs one statement that returns no rows: a DDL statement of `dbgen apply`, as
    /// written, or a statement of dbgen's.
    fn execute(&self, sql: &str) -> DbResult<()>;

    /// Runs a query and returns every value as text, `None` for NULL, in the order the
    /// server returns them.
    fn query_rows(&self, sql: &str) -> DbResult<TextRows>;

    /// Every table of the database with its columns in order, generated columns included,
    /// each with its type as the catalog spells it ([`parse_type`](Backend::parse_type)
    /// reads it back when [`strict_types`](Backend::strict_types) is set).
    fn live_columns(&self) -> DbResult<LiveColumns>;

    /// Appends one row to `out` in the backend's bulk format, ready for
    /// [`insert`](Backend::insert). Every value must reach the database as it is: a value
    /// the column cannot hold exactly is a [`CellError`], never rounded or truncated
    /// silently. Called on generator threads, many at a time.
    fn encode_row(&self, out: &mut Vec<u8>, row: &RowRef<'_>) -> std::result::Result<(), CellError>;

    /// Inserts a batch of rows encoded by [`encode_row`](Backend::encode_row) into
    /// `columns` of `table`, in that order. Atomic: on success every row is written, on
    /// failure none is, because core retries a failed batch whole and resumes by whole
    /// batches. A failure is [`Transient`](DbErrorKind::Transient) only if nothing was
    /// written.
    fn insert(&self, table: &Table, columns: &[&str], batch: &Batch) -> DbResult<()>;

    /// Removes every row of `tables`. Foreign keys among them, or from other tables to
    /// them, do not get in the way.
    fn truncate(&self, tables: &[&str]) -> DbResult<()>;

    /// Runs after a successful load of `tables`: advances sequences past the loaded keys,
    /// refreshes statistics. The default does nothing.
    fn finish_load(&self, _tables: &[&Table]) -> DbResult<()> {
        Ok(())
    }

    /// The DDL of the live database, as a script of `;`-terminated statements in the
    /// connector's dialect, in an order [`schema::compile`](crate::schema::compile) can
    /// fold: types first, then tables, then foreign keys and unique indexes. dbgen uses it
    /// when the configuration gives no DDL files. The default says the connector cannot
    /// read its schema.
    ///
    /// Compiled by the connector's own dialect, the script must give the schema that the
    /// DDL which built the database gives: the same tables, columns in order with their
    /// types and nullability, keys, foreign keys and CHECKs. Constraint names, comments
    /// and seeded rows may be lost. Where the catalog spells something differently from
    /// the declaration and dbgen would read it differently (`tinyint(1)` for `BOOLEAN`, a
    /// cast added to a CHECK), write the declared form back. The conformance scenario
    /// `introspection_round_trip` checks all of this.
    fn introspect_ddl(&self) -> DbResult<String> {
        Err(DbError::failed(format!(
            "the {} connector cannot read the schema from the database; give the DDL files with `schema` or --schema",
            self.dialect().name()
        )))
    }

    /// Connections that may write at the same time: `workers` is capped to it. The
    /// default is no limit; a database with a single writer says 1.
    fn max_writers(&self) -> usize {
        usize::MAX
    }

    /// Whether the bulk format depends on the exact column types, so the live types must
    /// match the DDL (a binary format) rather than just accept the values (a text format).
    /// The default: a text format.
    fn strict_types(&self) -> bool {
        false
    }

    /// Quotes an identifier. The default asks the connector
    /// ([`Connector::quote_ident`](crate::connector::Connector::quote_ident)).
    fn quote(&self, ident: &str) -> String {
        self.dialect().quote_ident(ident)
    }

    /// A table name as queries must spell it. The default quotes it; a backend that
    /// qualifies names with the database overrides it.
    fn table_ref(&self, table: &str) -> String {
        self.quote(table)
    }

    /// Parses a type as the catalog spells it. The default asks the connector
    /// ([`Connector::parse_type`](crate::connector::Connector::parse_type)).
    fn parse_type(&self, s: &str) -> std::result::Result<DataType, String> {
        self.dialect().parse_type(s)
    }

    /// `expr` cast to text, for [`read_rows`](Backend::read_rows). The default is
    /// `CAST(expr AS TEXT)`.
    fn cast_to_text(&self, expr: &str) -> String {
        format!("CAST({expr} AS TEXT)")
    }

    /// Runs a query returning one non-negative integer.
    fn query_u64(&self, sql: &str) -> DbResult<u64> {
        let rows = self.query_rows(sql)?;
        let cell = rows.first().and_then(|r| r.first()).cloned().flatten();
        cell.as_deref()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| DbError::failed(format!("unexpected result {cell:?} for: {sql}")))
    }

    /// The exact number of rows of `table`, as a fresh read sees them: resume compares it
    /// with the rows it wrote, so an estimate would reload tables for nothing.
    fn count(&self, table: &str) -> DbResult<u64> {
        self.query_u64(&format!("SELECT COUNT(*) FROM {}", self.table_ref(table)))
    }

    /// Up to `limit` distinct rows of `columns`, in a deterministic order: the rows of
    /// tables that are referenced but not generated, which generated rows then point at.
    /// The same data must give the same rows in the same order, or the generated rows
    /// change.
    fn read_rows(&self, table: &Table, columns: &[usize], limit: u64) -> DbResult<Vec<Vec<Value>>> {
        let list: Vec<String> = columns.iter().map(|&c| self.cast_to_text(&self.quote(&table.columns[c].name))).collect();
        let order: Vec<String> = (1..=columns.len()).map(|i| i.to_string()).collect();
        let sql = format!("SELECT DISTINCT {} FROM {} ORDER BY {} LIMIT {limit}", list.join(", "), self.table_ref(&table.name), order.join(", "));
        let rows = self.query_rows(&sql)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .zip(columns)
                    .map(|(cell, &c)| cell.map_or(Value::Null, |s| cast(Value::Str(s), &table.columns[c].ty)))
                    .collect()
            })
            .collect())
    }

    /// Counts rows beyond the first for each value of `key` (0 = the key is unique).
    fn duplicates_sql(&self, table: &Table, key: &[String]) -> String {
        let t = self.table_ref(&table.name);
        format!("SELECT (SELECT COUNT(*) FROM {t}) - (SELECT COUNT(*) FROM (SELECT DISTINCT {} FROM {t}) AS d)", key.join(", "))
    }

    /// Counts rows lost to background merges, for engines that merge rows. The default:
    /// none do.
    fn collapsed_sql(&self, _table: &Table) -> Option<String> {
        None
    }

    /// Counts child rows whose (non-NULL) reference has no parent row.
    fn orphans_sql(&self, child: &Table, child_cols: &[&str], parent: &Table, parent_cols: &[&str]) -> String {
        let not_null: Vec<String> = child_cols.iter().map(|c| format!("c.{} IS NOT NULL", self.quote(c))).collect();
        let join: Vec<String> = child_cols.iter().zip(parent_cols).map(|(c, p)| format!("p.{} = c.{}", self.quote(p), self.quote(c))).collect();
        format!(
            "SELECT COUNT(*) FROM {} AS c WHERE {} AND NOT EXISTS (SELECT 1 FROM {} AS p WHERE {})",
            self.table_ref(&child.name),
            not_null.join(" AND "),
            self.table_ref(&parent.name),
            join.join(" AND ")
        )
    }

    /// Counts rows that violate a CHECK constraint.
    fn violations_sql(&self, table: &Table, check_sql: &str) -> String {
        format!("SELECT COUNT(*) FROM {} WHERE NOT ({check_sql})", self.table_ref(&table.name))
    }
}

/// A connected backend, of the configuration's connector.
pub struct Database(Box<dyn Backend>);

impl Database {
    /// Connects to the configured database. Connections are opened lazily, so this only
    /// fails on a malformed URL; [`Backend::version`] is the first call to reach the server.
    pub fn connect(config: &Config) -> Result<Database> {
        let url = config.database_url()?;
        config.dialect.connect(url, config).map(Database)
    }
}

impl Deref for Database {
    type Target = dyn Backend;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl crate::plan::RowSource for Database {
    fn read_rows(&self, table: &Table, columns: &[usize], limit: u64) -> Result<Vec<Vec<Value>>> {
        self.0.read_rows(table, columns, limit).map_err(|e| e.into_error(&format!("reading {}", table.name)))
    }
}

/// Replaces every character that is not safe in a file name, for
/// [`Backend::state_key`].
pub fn file_safe(s: &str) -> String {
    s.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' }).collect()
}

/// Groups `(table, column, type)` rows, as a catalog query returns them, by table: the
/// result of [`Backend::live_columns`].
pub fn group_columns(rows: TextRows) -> LiveColumns {
    let mut out = LiveColumns::new();
    for row in rows {
        if let [Some(t), Some(c), Some(ty)] = row.as_slice() {
            out.entry(t.clone()).or_default().push((c.clone(), ty.clone()));
        }
    }
    out
}
