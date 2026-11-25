//! The PostgreSQL backend, over the native protocol without TLS. Rows are loaded with
//! `COPY ... FROM STDIN` in text format, one `COPY` per batch.

use std::io::Write as _;

use postgres::config::{Host, SslMode};
use postgres::{NoTls, SimpleQueryMessage};

use crate::connector::Dialect;
use crate::db::pool::Pool;
use crate::db::text::copy_field;
use crate::db::{file_safe, group_columns, Backend, Batch, CellError, DbError, DbErrorKind, DbResult, LiveColumns, RowRef, TextRows};
use crate::error::{Error, Result};
use crate::schema::Table;
use crate::sql::DefaultKind;

/// A PostgreSQL database, reached through a pool of connections.
pub struct Postgres {
    config: postgres::Config,
    pool: Pool<postgres::Client>,
    host: String,
    port: u16,
    database: String,
}

impl Postgres {
    /// `postgres://user:password@host:5432/database`, or libpq's `key=value` form.
    /// `sslmode=require` and stricter modes are refused: dbgen connects without TLS.
    pub fn connect(url: &str) -> Result<Postgres> {
        let config: postgres::Config = url.parse().map_err(|e| Error::Config(format!("database URL: {e}")))?;
        if !matches!(config.get_ssl_mode(), SslMode::Disable | SslMode::Prefer) {
            return Err(Error::Config("database URL: sslmode=require is not supported; dbgen connects to PostgreSQL without TLS".into()));
        }
        let host = match config.get_hosts().first() {
            Some(Host::Tcp(h)) => h.clone(),
            Some(Host::Unix(p)) => p.display().to_string(),
            None => "localhost".into(),
        };
        let port = config.get_ports().first().copied().unwrap_or(5432);
        let database = config.get_dbname().or(config.get_user()).unwrap_or("postgres").to_string();
        let pool_config = config.clone();
        let pool = Pool::new(move || open(&pool_config));
        Ok(Postgres { config, pool, host, port, database })
    }
}

fn open(config: &postgres::Config) -> DbResult<postgres::Client> {
    let mut client = config.connect(NoTls).map_err(classify)?;
    // Timestamps are written in UTC and dates in ISO order, whatever the server defaults.
    client.batch_execute("SET TIME ZONE 'UTC'; SET datestyle = 'ISO, YMD'").map_err(classify)?;
    Ok(client)
}

/// Maps a driver error onto what the caller should do about it, keeping the SQLSTATE
/// and the server's detail in the message.
fn classify(e: postgres::Error) -> DbError {
    let Some(db) = e.as_db_error() else {
        let kind = if e.is_closed() || e.to_string().contains("connect") { DbErrorKind::Unreachable } else { DbErrorKind::Transient };
        return DbError::new(kind, e.to_string());
    };
    let code = db.code().code();
    let mut message = format!("{} (SQLSTATE {code})", db.message());
    if let Some(detail) = db.detail() {
        message.push_str(": ");
        message.push_str(detail);
    }
    let kind = match code {
        "3D000" => DbErrorKind::UnknownDatabase,
        // Serialization failures and deadlocks, insufficient resources, shutdowns, lost connections.
        c if c.starts_with("40") || c.starts_with("53") || c.starts_with("57P") || c.starts_with("08") => DbErrorKind::Transient,
        _ => DbErrorKind::Failed,
    };
    DbError::new(kind, message)
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Backend for Postgres {
    fn dialect(&self) -> Dialect {
        super::CONNECTOR
    }

    fn describe(&self) -> String {
        format!("postgres {}:{}/{}", self.host, self.port, self.database)
    }

    fn state_key(&self) -> String {
        file_safe(&format!("{}_{}_{}", self.host, self.port, self.database))
    }

    fn version(&self) -> DbResult<String> {
        self.query_rows("SHOW server_version").map(|rows| rows.into_iter().next().and_then(|r| r.into_iter().next()).flatten().unwrap_or_default())
    }

    fn create_database(&self) -> DbResult<()> {
        let mut admin = self.config.clone();
        admin.dbname("postgres");
        let mut client = open(&admin)?;
        let exists = client.simple_query(&format!("SELECT 1 FROM pg_database WHERE datname = {}", literal(&self.database))).map_err(classify)?;
        if !exists.iter().any(|m| matches!(m, SimpleQueryMessage::Row(_))) {
            client.batch_execute(&format!("CREATE DATABASE {}", self.quote(&self.database))).map_err(classify)?;
        }
        Ok(())
    }

    fn execute(&self, sql: &str) -> DbResult<()> {
        self.pool.with(|c| c.batch_execute(sql).map_err(classify))
    }

    fn query_rows(&self, sql: &str) -> DbResult<TextRows> {
        let messages = self.pool.with(|c| c.simple_query(sql).map_err(classify))?;
        Ok(messages
            .into_iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(row) => Some((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect()),
                _ => None,
            })
            .collect())
    }

    fn live_columns(&self) -> DbResult<LiveColumns> {
        let rows = self.query_rows(
            "SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod)
             FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped
             ORDER BY c.relname, a.attnum",
        )?;
        Ok(group_columns(rows))
    }

    fn encode_row(&self, out: &mut Vec<u8>, row: &RowRef<'_>) -> std::result::Result<(), CellError> {
        for (k, ty, v) in row.cells() {
            if k > 0 {
                out.push(b'\t');
            }
            copy_field(out, ty, v).map_err(|message| CellError { column: k, message })?;
        }
        out.push(b'\n');
        Ok(())
    }

    fn insert(&self, table: &Table, columns: &[&str], batch: &Batch) -> DbResult<()> {
        let cols: Vec<String> = columns.iter().map(|c| self.quote(c)).collect();
        let sql = format!("COPY {} ({}) FROM STDIN", self.table_ref(&table.name), cols.join(", "));
        self.pool.with(|c| {
            let mut writer = c.copy_in(&sql).map_err(classify)?;
            writer.write_all(&batch.body).map_err(|e| DbError::transient(format!("sending rows: {e}")))?;
            writer.finish().map_err(classify)?;
            Ok(())
        })
    }

    fn truncate(&self, tables: &[&str]) -> DbResult<()> {
        if tables.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = tables.iter().map(|t| self.table_ref(t)).collect();
        self.execute(&format!("TRUNCATE TABLE {}", list.join(", ")))
    }

    /// Moves the sequences of serial and identity columns past the loaded keys, so rows
    /// inserted later by applications do not collide with them, and refreshes statistics.
    fn finish_load(&self, tables: &[&Table]) -> DbResult<()> {
        for t in tables {
            let table = self.table_ref(&t.name);
            for c in t.columns.iter().filter(|c| matches!(c.default.as_ref().map(|d| d.kind), Some(DefaultKind::Identity))) {
                let col = self.quote(&c.name);
                self.execute(&format!(
                    "SELECT setval(pg_get_serial_sequence({}, {}), MAX({col})) FROM {table} HAVING MAX({col}) IS NOT NULL",
                    literal(&table),
                    literal(&c.name)
                ))?;
            }
            self.execute(&format!("ANALYZE {table}"))?;
        }
        Ok(())
    }

    /// The schema of `current_schema()`, rebuilt from the catalogs (see
    /// [`introspect`](super::introspect)).
    fn introspect_ddl(&self) -> DbResult<String> {
        super::introspect::ddl(|sql| self.query_rows(sql))
    }
}
