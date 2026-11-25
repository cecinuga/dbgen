//! The MySQL and MariaDB backend, over the native protocol without TLS. Rows are loaded
//! with multi-row `INSERT` statements, all statements of a batch in one transaction.

use std::net::ToSocketAddrs;

use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder};

use crate::connector::Dialect;
use crate::db::pool::Pool;
use crate::db::text::{insert_statements, sql_row, TextStyle};
use crate::db::{file_safe, group_columns, Backend, Batch, CellError, DbError, DbErrorKind, DbResult, LiveColumns, RowRef, TextRows};
use crate::error::{Error, Result};
use crate::schema::Table;

/// Statements stay well below the default `max_allowed_packet` (64 MiB).
const MAX_STATEMENT_BYTES: usize = 4 << 20;

/// Booleans as `1`/`0`; strings escaped with backslashes, as the session's `sql_mode`
/// (without `NO_BACKSLASH_ESCAPES`) reads them.
const STYLE: TextStyle = TextStyle { true_text: "1", false_text: "0", backslash_escapes: true };

/// A MySQL database, reached through a pool of connections.
pub struct Mysql {
    opts: Opts,
    pool: Pool<Conn>,
    host: String,
    port: u16,
    database: String,
}

impl Mysql {
    /// `mysql://user:password@host:3306/database`, or `mariadb://...`.
    pub fn connect(url: &str) -> Result<Mysql> {
        let url = match url.strip_prefix("mariadb://") {
            Some(rest) => format!("mysql://{rest}"),
            None => url.to_string(),
        };
        let opts = Opts::from_url(&url).map_err(|e| Error::Config(format!("database URL: {e}")))?;
        let database = opts.get_db_name().ok_or_else(|| Error::Config("database URL: no database name".into()))?.to_string();
        let (host, port) = (opts.get_ip_or_hostname().to_string(), opts.get_tcp_port());
        let pool_opts = opts.clone();
        let pool = Pool::new(move || open(pool_opts.clone()));
        Ok(Mysql { opts, pool, host, port, database })
    }
}

/// Opens a connection, trying every address the host resolves to: the driver tries only
/// the first, and `localhost` often resolves to `::1` before `127.0.0.1`.
fn open(opts: Opts) -> DbResult<Conn> {
    let host = opts.get_ip_or_hostname().to_string();
    let addrs: Vec<_> = (host.as_str(), opts.get_tcp_port()).to_socket_addrs().map(Iterator::collect).unwrap_or_default();
    let mut last = DbError::new(DbErrorKind::Unreachable, format!("cannot resolve {host}"));
    for addr in addrs {
        match Conn::new(OptsBuilder::from_opts(opts.clone()).ip_or_hostname(Some(addr.ip().to_string()))) {
            Ok(conn) => return start_session(conn),
            Err(mysql::Error::MySqlError(e)) => return Err(classify(mysql::Error::MySqlError(e))),
            Err(e) => last = DbError::new(DbErrorKind::Unreachable, e.to_string()),
        }
    }
    Err(last)
}

fn start_session(mut conn: Conn) -> DbResult<Conn> {
    // Timestamps are written in UTC, and string literals use backslash escapes.
    conn.query_drop("SET time_zone = '+00:00', SESSION sql_mode = REPLACE(@@SESSION.sql_mode, 'NO_BACKSLASH_ESCAPES', '')").map_err(classify)?;
    Ok(conn)
}

/// Maps a driver error onto what the caller should do about it.
fn classify(e: mysql::Error) -> DbError {
    match &e {
        mysql::Error::MySqlError(m) => {
            let kind = match m.code {
                1049 => DbErrorKind::UnknownDatabase,
                // Deadlock, lock wait timeout, too many connections, server shutting down.
                1213 | 1205 | 1040 | 1053 => DbErrorKind::Transient,
                _ => DbErrorKind::Failed,
            };
            DbError::new(kind, format!("{} (error {})", m.message, m.code))
        }
        mysql::Error::IoError(_) | mysql::Error::CodecError(_) => DbError::new(DbErrorKind::Unreachable, e.to_string()),
        mysql::Error::DriverError(_) => DbError::transient(e.to_string()),
        _ => DbError::failed(e.to_string()),
    }
}

fn cell(v: mysql::Value) -> Option<String> {
    match v {
        mysql::Value::NULL => None,
        mysql::Value::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        mysql::Value::Int(i) => Some(i.to_string()),
        mysql::Value::UInt(u) => Some(u.to_string()),
        mysql::Value::Float(f) => Some(f.to_string()),
        mysql::Value::Double(d) => Some(d.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

impl Backend for Mysql {
    fn dialect(&self) -> Dialect {
        super::CONNECTOR
    }

    fn describe(&self) -> String {
        format!("mysql {}:{}/{}", self.host, self.port, self.database)
    }

    fn state_key(&self) -> String {
        file_safe(&format!("{}_{}_{}", self.host, self.port, self.database))
    }

    fn version(&self) -> DbResult<String> {
        self.query_rows("SELECT VERSION()").map(|rows| rows.into_iter().next().and_then(|r| r.into_iter().next()).flatten().unwrap_or_default())
    }

    fn create_database(&self) -> DbResult<()> {
        let mut conn = open(Opts::from(OptsBuilder::from_opts(self.opts.clone()).db_name(None::<String>)))?;
        conn.query_drop(format!("CREATE DATABASE IF NOT EXISTS {}", self.quote(&self.database))).map_err(classify)
    }

    fn execute(&self, sql: &str) -> DbResult<()> {
        self.pool.with(|c| c.query_drop(sql).map_err(classify))
    }

    fn query_rows(&self, sql: &str) -> DbResult<TextRows> {
        let rows: Vec<mysql::Row> = self.pool.with(|c| c.query(sql).map_err(classify))?;
        Ok(rows.into_iter().map(|r| r.unwrap().into_iter().map(cell).collect()).collect())
    }

    fn live_columns(&self) -> DbResult<LiveColumns> {
        let rows = self.query_rows(
            "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, ORDINAL_POSITION",
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
        self.pool.with(|c| {
            let mut tx = c.start_transaction(mysql::TxOpts::default()).map_err(classify)?;
            for sql in &statements {
                tx.query_drop(sql).map_err(classify)?;
            }
            tx.commit().map_err(classify)
        })
    }

    fn truncate(&self, tables: &[&str]) -> DbResult<()> {
        self.pool.with(|c| {
            c.query_drop("SET FOREIGN_KEY_CHECKS = 0").map_err(classify)?;
            let result = tables.iter().try_for_each(|t| c.query_drop(format!("TRUNCATE TABLE {}", self.table_ref(t))).map_err(classify));
            c.query_drop("SET FOREIGN_KEY_CHECKS = 1").map_err(classify)?;
            result
        })
    }

    fn finish_load(&self, tables: &[&Table]) -> DbResult<()> {
        tables.iter().try_for_each(|t| self.query_rows(&format!("ANALYZE TABLE {}", self.table_ref(&t.name))).map(|_| ()))
    }

    /// MySQL casts to `CHAR`, not `TEXT`.
    fn cast_to_text(&self, expr: &str) -> String {
        format!("CAST({expr} AS CHAR)")
    }

    /// `SHOW CREATE TABLE` of every base table (see [`introspect`](super::introspect)).
    fn introspect_ddl(&self) -> DbResult<String> {
        super::introspect::ddl(|sql| self.query_rows(sql), |t| self.table_ref(t))
    }
}

