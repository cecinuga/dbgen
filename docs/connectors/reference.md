# Reference

This page lists every method a connector implements or inherits, with a one-line contract and the built-in
connectors that override it. The full contracts are in the rustdoc of
[`Connector`](../../src/connector.rs) and [`Backend`](../../src/db/mod.rs). The second half of the page walks
through the built-in connectors as worked examples.

## `Connector`: names, dialect, capabilities

A unit struct per database. It is registered as a `Dialect` handle (`pub const CONNECTOR: Dialect =
Dialect::new(&MyConnector)`) and called from any thread.

| Method | Required | Contract | Overridden by |
|---|---|---|---|
| `name` | yes | Lowercase, unique, stable: the `dialect:` value, `--dialect`, fixture file names. | all |
| `product` | yes | The product name, for messages. | all |
| `aliases` | no (none) | Other accepted names, lowercase, unique across connectors. | all |
| `schemes` | yes | URL schemes that select the connector, lowercase, unique across connectors. | all |
| `connect` | yes | Parses the URL, connects lazily, fails only on a malformed URL (`Error::Config`). Reads its settings from `&Config`. | all |
| `sql` | yes | The `sqlparser` dialect that tokenizes and parses the DDL. | all |
| `type_rules` | yes | How type spellings and column definitions read, including custom type names. See [type mapping](types.md). | all |
| `split` | no (top-level `;`) | DDL file to statements. | none |
| `parse_statement` | no (shared adapter) | One statement to `Ddl` commands. Statements that cannot change table structure become `Ddl::Ignored`. | ClickHouse |
| `parse_type` | no (shared adapter) | A type string, from DDL or from the catalog, to a `DataType`. | ClickHouse |
| `quote_ident` | no (ANSI `"x"`) | An identifier that reads back as itself. | MySQL, ClickHouse |
| `table_semantics` | no (nothing) | Keys and fixed column values implied by a table engine. | ClickHouse |
| `is_storage_table` | no (`true`) | Whether a `CREATE TABLE` makes a table that stores generated rows. | ClickHouse |
| `enforces_foreign_keys` | no (`true`) | Whether inserts check foreign keys: parents then load first, and resume reloads dependents. | ClickHouse (`false`) |

## `Backend`: one connected database

One value per run, shared by every writer thread (`Send + Sync`). Every fallible method returns
`DbResult<T>`, whose error carries a `DbErrorKind` (see
[the guarantees](../CONNECTORS.md#the-guarantees-core-relies-on)).

| Method | Required | Contract | Overridden by |
|---|---|---|---|
| `dialect` | yes | The connector's handle (`CONNECTOR`). | all |
| `describe` | yes | Target for messages, no password: `postgres localhost:5432/shop`. | all |
| `state_key` | yes | File-name-safe identity of server and database, for the resume state. Use `file_safe`. | all |
| `version` | yes | First call to reach the server; `UnknownDatabase` / `Unreachable` when it cannot. | all |
| `create_database` | yes | Creates the database if missing; succeeds if it exists. | all |
| `execute` | yes | One statement, no rows: a DDL statement as written, or one of dbgen's. | all |
| `query_rows` | yes | A query's rows, every value as text, `None` for NULL. | all |
| `live_columns` | yes | Every table and column in order, generated columns included, types as the catalog spells them. | all |
| `encode_row` | yes | One row in the bulk format, exactly as given, or a `CellError` naming the column. Runs on generator threads. | all |
| `insert` | yes | A batch into the given columns, atomically. `Transient` only if nothing was written. | all |
| `truncate` | yes | Empties tables; foreign keys do not get in the way. | all |
| `finish_load` | no (nothing) | Maintenance after a successful load: sequences, statistics. | PostgreSQL, MySQL, SQLite |
| `introspect_ddl` | no (an error saying the connector cannot read its schema) | The live schema as a DDL script in the connector's dialect, for runs without DDL files. See [schema introspection](#schema-introspection). | all |
| `max_writers` | no (no limit) | Writers allowed at once; `workers` is capped to it. | SQLite (1) |
| `strict_types` | no (`false`) | Whether live column types must equal the DDL's (a binary format). | ClickHouse |
| `quote` | no (`Connector::quote_ident`) | Quotes an identifier in queries. | ClickHouse |
| `table_ref` | no (quoted name) | A table as queries spell it. | ClickHouse (`db`.`table`) |
| `parse_type` | no (`Connector::parse_type`) | A catalog type string to a `DataType`. | none |
| `cast_to_text` | no (`CAST(x AS TEXT)`) | An expression cast to text, for `read_rows`. | MySQL (`AS CHAR`) |
| `query_u64` | no (first cell of `query_rows`) | A query returning one non-negative integer. | ClickHouse |
| `count` | no (`SELECT COUNT(*)`) | The exact row count, as a fresh read sees it. | none |
| `read_rows` | no (`SELECT DISTINCT ... ORDER BY ... LIMIT`) | Up to `limit` distinct rows, deterministic order, values cast to the column types. | ClickHouse |
| `duplicates_sql` | no (portable SQL) | Counts rows beyond the first per key value. | ClickHouse |
| `collapsed_sql` | no (`None`) | Counts rows lost to background merges, for engines that merge. | ClickHouse |
| `orphans_sql` | no (`NOT EXISTS`) | Counts non-NULL references without a parent. | ClickHouse |
| `violations_sql` | no (`WHERE NOT (check)`) | Counts rows violating a CHECK. | ClickHouse |

### Types a backend handles

| Type | In [`src/db/mod.rs`](../../src/db/mod.rs) | Use |
|---|---|---|
| `RowRef` | `types`, `values`, `columns` | One row to encode: `cells()` yields `(position, &DataType, &Value)` in insert order. |
| `Batch` | `body`, `row_ends` | Encoded rows. `row_ends` lets a backend split a batch into statements (`row_slices()`). Core pushes the row ends; `encode_row` only appends to `body`. |
| `CellError` | `column`, `message` | A value that cannot be encoded; `column` is the position in insert order. |
| `DbError`, `DbErrorKind` | `DbError::new`, `::failed`, `::transient` | A failed call and what core should do about it. |
| `LiveColumns` | `HashMap<table, Vec<(column, type)>>` | The result of `live_columns`. `group_columns` builds it from catalog rows. |
| `TextRows` | `Vec<Vec<Option<String>>>` | The result of `query_rows`. |

## Schema introspection

`Backend::introspect_ddl` is optional. Without it, users of the connector must give DDL files (`schema`). With
it, dbgen can read the schema from the database itself. It returns one script:

- **In the connector's own dialect.** dbgen compiles it with the connector's `split` and `parse_statement`,
  exactly as it compiles DDL files, so whatever the catalog prints must parse there.
- **Of `;`-terminated statements, dependencies first**: types and domains, tables, then foreign keys and unique
  indexes (`ALTER TABLE ... ADD CONSTRAINT`, `CREATE UNIQUE INDEX`). dbgen does not check references at
  compile time, but a script in that order can also be applied to another database as it is.
- **Of the current database only**: no system tables, no partitions of a partitioned table, no hidden tables.

What must survive the trip is what dbgen generates from: the tables, every column in order with its type and
nullability, the primary and unique keys, the foreign keys, and the CHECKs. Constraint names, comments, plain
indexes and seeded rows do not matter. The rows of seeded tables are read from the database anyway, as
`existing` tables.

Catalogs normalize what they print. When the printed form compiles to something other than the declaration,
write the declared form back, so the introspected schema generates the same rows:

| Connector | Reads | Writes back |
|---|---|---|
| PostgreSQL ([`introspect.rs`](../../src/connectors/postgres/introspect.rs)) | `pg_type`, `pg_attribute`, `pg_constraint`, `pg_index` of `current_schema()`, through `format_type`, `pg_get_expr`, `pg_get_constraintdef`, `pg_get_indexdef` | `timestamp with time zone` as `timestamptz`; a sequence default owned by its column as `serial`; CHECKs without the casts PostgreSQL adds (`(0)::numeric`, `'x'::text`) and with `x = ANY (ARRAY[...])` as `x IN (...)`, parsed and rewritten with `sqlparser` |
| MySQL ([`introspect.rs`](../../src/connectors/mysql/introspect.rs)) | `SHOW CREATE TABLE` of each base table, parents first | `tinyint(1)` as `boolean`; `DEFAULT NULL` dropped (dbgen would read it as a database default); character set introducers (`_utf8mb4'x'`) dropped |
| SQLite (`backend.rs`) | `sqlite_schema.sql` of tables, then indexes | nothing: SQLite keeps the statements as written |
| ClickHouse (`backend.rs`) | `system.tables.create_table_query`: tables, then views and materialized views | nothing: the adapter reads ClickHouse's canonical form; `.inner` tables are skipped, and `TO target` keeps `derived_by` |

The `introspection_round_trip` scenario of the [conformance suite](testing.md#the-conformance-suite) checks all
of this on every fixture. It applies the fixture, introspects the database, and compiles both. It then compares
their tables (with engines and materialized-view targets), columns, keys, foreign keys and CHECK counts. It
also plans both, requiring the same warnings and the same repairs in a sample of rows. That last check catches
a CHECK whose bounds or values the catalog form hides from dbgen.

## The toolkit

Code connectors share. It names no database.

| Item | What |
|---|---|
| [`dialect::standard`](../../src/dialect/standard.rs) | The DDL adapter for dialects `sqlparser` parses whole: `parse_statement(stmt, sql, rules)`, `parse_type(s, sql, rules)`. The default of `Connector::parse_statement` and `parse_type`. |
| [`sql::convert`](../../src/sql/convert.rs) | `TypeRules` and `TypeRules::STANDARD`; `data_type`, `column`, `table_constraint` for custom adapters. |
| [`sql`](../../src/sql/mod.rs) | `split`, `Statement`, `Ddl`, and token helpers (`is_kw`, `read_name`, `parse_slice`...) for custom adapters. |
| [`db::pool::Pool`](../../src/db/pool.rs) | A pool for drivers whose client holds one connection. `with(|conn| ...)` runs a call; a connection that failed transiently is dropped. |
| [`db::text`](../../src/db/text.rs) | `copy_field` (PostgreSQL `COPY` text format), `sql_row` / `sql_literal` with a `TextStyle`, `insert_statements` (multi-row `INSERT`s under a size limit), `scalar_text`. |
| [`db::group_columns`](../../src/db/mod.rs), [`db::file_safe`](../../src/db/mod.rs) | Catalog rows to `LiveColumns`; a string made safe for a file name. |

A `TextStyle` is three settings, so a connector states its own (this block is a doctest):

```rust
use dbgen::db::text::{sql_literal, TextStyle};
use dbgen::schema::DataType;
use dbgen::value::Value;

const STYLE: TextStyle = TextStyle { true_text: "TRUE", false_text: "FALSE", backslash_escapes: false };

let text = DataType::String { max_len: None };
let mut out = Vec::new();
sql_literal(&mut out, &text, &Value::Str("it's".into()), STYLE).unwrap();
out.push(b',');
sql_literal(&mut out, &DataType::Bool, &Value::Bool(true), STYLE).unwrap();
assert_eq!(String::from_utf8(out).unwrap(), "'it''s',TRUE");
```

## Worked examples

### PostgreSQL: the typical SQL connector

[`src/connectors/postgres/`](../../src/connectors/postgres/): `mod.rs` is the `Connector`, `backend.rs` the
`Backend`.

- **DDL** goes through the shared adapter with `PostgreSqlDialect`. `RULES` is `TypeRules::STANDARD` with a
  `custom_type` for the names `sqlparser` leaves custom (`serial`, `inet`, `citext`, `money`, `timestamptz`)
  and `identity_types` for the `serial` family: those columns become `NOT NULL` with an identity default.
- **Connections**: a `Pool<postgres::Client>`. Each new connection runs `SET TIME ZONE 'UTC'; SET datestyle =
  'ISO, YMD'`, so timestamps written as text mean UTC.
- **Errors** are classified by SQLSTATE: `3D000` is `UnknownDatabase`; classes `40` (serialization, deadlock),
  `53` (resources), `57P` (shutdown) and `08` (connection) are `Transient`; anything else is `Failed`.
- **Loading**: `encode_row` writes `COPY` text with `copy_field`, and `insert` sends the batch as one
  `COPY ... FROM STDIN`, which is atomic.
- **Catalog**: `pg_attribute` and `format_type` for the tables of `current_schema()`.
- **After the load**: `finish_load` moves each identity column's sequence past the loaded keys (`setval`), so
  later application inserts do not collide, then runs `ANALYZE`.
- **Introspection**: PostgreSQL keeps no DDL text, so `introspect.rs` rebuilds the script from the catalogs and
  writes PostgreSQL's deparsed forms back in their declared spelling (see
  [schema introspection](#schema-introspection)).

### MySQL: text inserts under a size limit

[`src/connectors/mysql/`](../../src/connectors/mysql/)

- `quote_ident` uses backticks; `cast_to_text` casts to `CHAR`, because MySQL has no `CAST(x AS TEXT)`.
- **Loading**: `sql_row` with `TextStyle { "1", "0", backslash_escapes: true }`. Each new connection removes
  `NO_BACKSLASH_ESCAPES` from `sql_mode`, so the escapes mean what the style assumes. `insert_statements` cuts
  the batch into statements of at most 4 MiB, below `max_allowed_packet`. One transaction wraps them, so the
  batch is atomic.
- **Truncate**: `SET FOREIGN_KEY_CHECKS = 0` around `TRUNCATE TABLE`, on the same connection.
- **Connections**: the driver tries only the first address of a host name, and `localhost` often resolves to
  `::1` first, so `open` tries every address itself.

### SQLite: a file and a single writer

[`src/connectors/sqlite/`](../../src/connectors/sqlite/)

- **URL**: `sqlite:path`. `connect` resolves a relative path against the configuration's directory
  (`config.base_dir`), and refuses `:memory:`, which would not outlive the run. `create_database` creates the
  directory and opens the file, which creates the database. Opening a missing file creates it too, so its tests
  opt out of `unknown_database`.
- **One writer**: `max_writers` is 1. Each connection sets `busy_timeout`, so writers wait for each other rather
  than fail, and `foreign_keys = ON`, because SQLite leaves foreign key checks off by default.
- **Catalog**: `pragma_table_xinfo`, not `pragma_table_info`, which leaves out generated columns.
- **Truncate**: `DELETE FROM` each table with `PRAGMA foreign_keys = OFF` around it, since SQLite has no
  `TRUNCATE`.

### ClickHouse: a custom DDL adapter and a binary format

[`src/connectors/clickhouse/`](../../src/connectors/clickhouse/): `dialect.rs` is the DDL adapter, `backend.rs`
the HTTP client.

- **DDL**: `sqlparser` parses ClickHouse types, columns and expressions, but not the structure of its `CREATE
  TABLE`: clauses in any order, `CODEC`, `TTL`, `INDEX ... TYPE`, `PROJECTION`, `CHECK` without parentheses.
  `parse_statement` walks the statement on `sqlparser`'s tokens, using the token helpers of `crate::sql`, and
  hands each column definition, type and expression back to `sqlparser`. `parse_type` parses a catalog type as
  a dummy column, so `DateTime('UTC')` goes through the same path.
- **Semantics**: `table_semantics` makes the sorting key of a merging engine (`ReplacingMergeTree`,
  `CollapsingMergeTree`...) a uniqueness key, since rows sharing it would merge away. It also fixes the values
  of its `is_deleted` and `sign` columns. `is_storage_table` excludes views, `Null`, `Distributed` and
  integration engines. `enforces_foreign_keys` is `false`, so tables load in any order.
- **Loading**: `encode_row` writes `RowBinary` from each column's `DataType` (`encode` in `backend.rs`). The
  format depends on the exact types, so `strict_types` is `true`, and core compares each live type, read back
  through `parse_type`, with the DDL's. One HTTP request per batch is atomic.
- **Queries**: `table_ref` qualifies names with the database. `read_rows` reads `RowBinary` too, with `FINAL`
  on merging tables. The verification SQL uses `uniqExact` and `NOT IN`, and `collapsed_sql` counts rows lost
  to merges.
- **Errors**: by server error code (`81` is `UnknownDatabase`, a list of codes is `Transient`), and by transport
  error (`ConnectionFailed` and `Dns` are `Unreachable`).

## Implementing the trait outside the crate

Connectors live in `src/connectors/`, but the API is public. This minimal `Connector` compiles from outside the
crate (paths start with `dbgen::` instead of `crate::`). It shows which items a connector imports:

```rust
use dbgen::config::Config;
use dbgen::connector::{Connector, Dialect};
use dbgen::db::Backend;
use dbgen::error::{Error, Result};
use dbgen::schema::DataType;
use dbgen::sql::convert::TypeRules;
use sqlparser::dialect::{Dialect as SqlDialect, GenericDialect};

struct Outline;

static SQL: GenericDialect = GenericDialect {};

impl Connector for Outline {
    fn name(&self) -> &'static str {
        "outline"
    }
    fn product(&self) -> &'static str {
        "Outline"
    }
    fn schemes(&self) -> &'static [&'static str] {
        &["outline"]
    }
    fn connect(&self, _url: &str, _config: &Config) -> Result<Box<dyn Backend>> {
        Err(Error::Config("an outline has no backend".into()))
    }
    fn sql(&self) -> &'static dyn SqlDialect {
        &SQL
    }
    fn type_rules(&self) -> TypeRules {
        TypeRules::STANDARD
    }
}

// Not registered, so `Dialect::from_name("outline")` does not find it, but a handle works.
let outline = Dialect::new(&Outline);
assert_eq!(outline.parse_type("VARCHAR(20)").unwrap(), DataType::String { max_len: Some(20) });
```
