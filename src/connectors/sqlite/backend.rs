//! The SQLite backend, through the bundled library. Rows are loaded with multi-row
//! `INSERT` statements in one transaction per batch. SQLite has a single writer, so
//! batches are inserted one at a time.

use std::path::{Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode};

use crate::connector::Dialect;
use crate::db::pool::Pool;
use crate::db::text::{insert_statements, sql_row, TextStyle};
use crate::db::{file_safe, group_columns, Backend, Batch, CellError, DbError, DbErrorKind, DbResult, LiveColumns, RowRef, TextRows};
use crate::error::{Error, Result};
use crate::schema::Table;

const MAX_STATEMENT_BYTES: usize = 1 << 20;

/// Booleans as `1`/`0` (SQLite has no boolean type); quotes doubled in strings.
const STYLE: TextStyle = TextStyle { true_text: "1", false_text: "0", backslash_escapes: false };

/// A SQLite database file.
pub struct Sqlite {
    path: PathBuf,
    pool: Pool<Connection>,
}

impl Sqlite {
    /// `sqlite:path/to/file.db`, `sqlite://relative.db` or `sqlite:///absolute.db`. A
    /// relative path is resolved against `base`, the configuration's directory.
    pub fn connect(url: &str, base: &Path) -> Result<Sqlite> {
        let rest = ["sqlite://", "sqlite3://", "file://", "sqlite:", "sqlite3:", "file:"]
            .iter()
            .find_map(|p| url.strip_prefix(p))
            .ok_or_else(|| Error::Config(format!("database URL: '{url}' is not sqlite:path")))?;
        let file = rest.split('?').next().unwrap_or_default();
        if file.is_empty() || file == ":memory:" {
            return Err(Error::Config("database URL: give SQLite a file path; an in-memory database would not outlive the run".into()));
        }
        let path = Path::new(file);
        let path = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
        let pool_path = path.clone();
        let pool = Pool::new(move || open(&pool_path));
        Ok(Sqlite { path, pool })
    }
}

fn open(path: &Path) -> DbResult<Connection> {
    let conn = Connection::open(path).map_err(|e| DbError::new(DbErrorKind::Unreachable, format!("{}: {e}", path.display())))?;
    // Foreign keys are off by default in SQLite; dbgen keeps them checked, like the
    // other databases. Writers wait for each other instead of failing.
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 30000;").map_err(classify)?;
    Ok(conn)
}

fn classify(e: rusqlite::Error) -> DbError {
    let kind = match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => DbErrorKind::Transient,
        Some(ErrorCode::CannotOpen) => DbErrorKind::Unreachable,
        _ => DbErrorKind::Failed,
    };
    DbError::new(kind, e.to_string())
}

fn cell(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    }
}

impl Backend for Sqlite {
    fn dialect(&self) -> Dialect {
        super::CONNECTOR
    }

    fn describe(&self) -> String {
        format!("sqlite {}", self.path.display())
    }

    fn state_key(&self) -> String {
        let name = self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned());
        file_safe(&format!("sqlite_{name}"))
    }

    fn version(&self) -> DbResult<String> {
        self.query_rows("SELECT sqlite_version()").map(|rows| rows.into_iter().next().and_then(|r| r.into_iter().next()).flatten().unwrap_or_default())
    }

    /// Opening a SQLite file creates it; only the directory may be missing.
    fn create_database(&self) -> DbResult<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DbError::failed(format!("creating {}: {e}", dir.display())))?;
        }
        self.pool.with(|_| Ok(()))
    }

    fn execute(&self, sql: &str) -> DbResult<()> {
        self.pool.with(|c| c.execute_batch(sql).map_err(classify))
    }

    fn query_rows(&self, sql: &str) -> DbResult<TextRows> {
        self.pool.with(|c| {
            let mut stmt = c.prepare(sql).map_err(classify)?;
            let n = stmt.column_count();
            let mut rows = stmt.query([]).map_err(classify)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(classify)? {
                out.push((0..n).map(|i| row.get_ref(i).map(cell).map_err(classify)).collect::<DbResult<Vec<_>>>()?);
            }
            Ok(out)
        })
    }

    /// `pragma_table_xinfo` rather than `pragma_table_info`, which leaves out generated
    /// columns (`hidden` 2 and 3); `hidden` 1 marks the hidden columns of virtual tables.
    fn live_columns(&self) -> DbResult<LiveColumns> {
        let rows = self.query_rows(
            "SELECT m.name, p.name, p.type FROM sqlite_schema AS m JOIN pragma_table_xinfo(m.name) AS p
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND p.hidden IN (0, 2, 3) ORDER BY m.name, p.cid",
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
            let tx = c.transaction().map_err(classify)?;
            for sql in &statements {
                tx.execute_batch(sql).map_err(classify)?;
            }
            tx.commit().map_err(classify)
        })
    }

    fn truncate(&self, tables: &[&str]) -> DbResult<()> {
        let deletes: String = tables.iter().map(|t| format!("DELETE FROM {};", self.table_ref(t))).collect();
        self.execute(&format!("PRAGMA foreign_keys = OFF; {deletes} PRAGMA foreign_keys = ON;"))
    }

    fn finish_load(&self, _tables: &[&Table]) -> DbResult<()> {
        self.execute("ANALYZE")
    }

    /// The statements SQLite keeps in `sqlite_schema`, as they were written (with later
    /// `ALTER TABLE`s folded in): tables in creation order, then their explicit indexes.
    /// Internal tables and the automatic indexes of constraints, which have no SQL, are
    /// left out. SQLite accepts references to tables created later, so the order of the
    /// tables needs no sorting.
    fn introspect_ddl(&self) -> DbResult<String> {
        let rows = self.query_rows(
            "SELECT sql FROM sqlite_schema WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
             ORDER BY type = 'index', rowid",
        )?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).map(|sql| format!("{};\n\n", sql.trim_end())).collect())
    }

    fn max_writers(&self) -> usize {
        1
    }
}
