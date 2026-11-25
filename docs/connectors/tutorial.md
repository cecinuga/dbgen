# Tutorial: writing a connector

This walks through a new connector from an empty module to a passing conformance suite. The running example is
a database called ExampleDB, with connector name `example`. Replace both with yours.

Code blocks marked *excerpt* are copied from [`src/connectors/template.rs`](../../src/connectors/template.rs).
That file compiles with the tests, so it cannot drift from the API. The other Rust blocks in this guide are
compiled as doctests.

## 0. Before you start

You need:

- **A toolchain.** `scripts/cargo-docker.sh` runs cargo in the official Rust image (`scripts/cargo-docker.sh
  build`, `... test`), so Docker is enough. A local `cargo` works too.
- **A server you can throw away.** A container is best: the tests create and drop databases named
  `dbgen_it_*_test`.
- **A driver crate.** dbgen is synchronous and uses threads, so prefer a blocking client. Prefer pure Rust with
  no system libraries: the built-in connectors build without OpenSSL headers or a C client.

Find out, for your database:

| Question | Where it goes |
|---|---|
| Which `sqlparser` dialect reads its DDL best (`PostgreSqlDialect`, `MySqlDialect`, `MsSqlDialect`, `GenericDialect`...)? | `Connector::sql` |
| How does its DDL spell types that `sqlparser` does not know? | `TypeRules::custom_type`, see [type mapping](types.md) |
| How do you load many rows fast, atomically? | `Backend::encode_row`, `Backend::insert` |
| How does it report errors: codes for deadlocks, a missing database, lost connections? | `classify`, see [the guarantees](../CONNECTORS.md#the-guarantees-core-relies-on) |
| Which catalog query lists every table and column with its type? | `Backend::live_columns` |
| Does it check foreign keys on insert? | `Connector::enforces_foreign_keys` |

## 1. Create the module

```sh
mkdir src/connectors/example
cp src/connectors/template.rs src/connectors/example/mod.rs
```

Rename `ExampleConnector` and `Example`. Delete the `#![allow(dead_code)]` line and the stand-in `Driver` at the
bottom, and use your driver crate instead. Keep the module's `//!` header up to date. Like every built-in
connector, it says in a few lines how DDL is read and how rows are loaded. Once the module grows, split it like
the built-in connectors: `mod.rs` for the `Connector`, `backend.rs` for the `Backend`.

## 2. Register it

Add one line to the registry in [`src/connectors/mod.rs`](../../src/connectors/mod.rs). The string is the Cargo
feature, the identifier is the module:

```rust,ignore
connectors! {
    "clickhouse" => clickhouse,
    "postgres" => postgres,
    "mysql" => mysql,
    "sqlite" => sqlite,
    "example" => example,
}
```

Add the feature, the driver as an optional dependency, and the test target to `Cargo.toml`:

```toml
[features]
default = ["clickhouse", "postgres", "mysql", "sqlite", "example"]
example = ["dep:example-driver"]

[dependencies]
example-driver = { version = "1.4", optional = true }

[[test]]
name = "example"
required-features = ["example"]
```

Whether the connector joins `default` is the maintainers' call. Usually it does, if the driver is light and
pure Rust. If the registry names a feature that `Cargo.toml` does not declare, the build warns about an
unexpected `cfg`.

Nothing else in dbgen needs to change: the `dialect:` key, URL schemes, `--dialect` and `--help` all read the
registry.

## 3. The `Connector` half

Six methods are required. The template's version of them (*excerpt*):

```rust,ignore
pub struct ExampleConnector;

pub const CONNECTOR: Dialect = Dialect::new(&ExampleConnector);

static SQL: GenericDialect = GenericDialect {};

const RULES: TypeRules = TypeRules { timestamp_precision: 0, ..TypeRules::STANDARD };

impl Connector for ExampleConnector {
    fn name(&self) -> &'static str { "example" }
    fn product(&self) -> &'static str { "ExampleDB" }
    fn schemes(&self) -> &'static [&'static str] { &["example"] }
    fn connect(&self, url: &str, _config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Example::connect(url)?))
    }
    fn sql(&self) -> &'static dyn SqlDialect { &SQL }
    fn type_rules(&self) -> TypeRules { RULES }
}
```

- `name` is the identity everywhere: `dialect: example`, `--dialect example`, the schema's JSON, and fixture
  file names (`tests/fixtures/<domain>/example.sql`). Lowercase, and never renamed once released. Add other
  spellings to `aliases`.
- `schemes` select the connector from `database: example://...` when the configuration has no `dialect`. A
  registry test rejects a scheme claimed twice, so do not claim generic ones such as `http`. ClickHouse already
  has it, and users of your connector can set `dialect:`.
- `connect` parses the URL and returns quickly. Open connections lazily: the template's `Pool` opens them on
  first use. A malformed URL is `Error::Config`.
- `type_rules` starts from `TypeRules::STANDARD` and changes what your DDL means differently. See
  [type mapping](types.md).

Then decide on the defaults:

| Method | Default | Override when |
|---|---|---|
| `aliases` | none | Users know the database by other names (`pg`, `mariadb`). |
| `split` | split at top-level `;` with `sql()`'s tokenizer; `BEGIN ... END` trigger bodies stay whole | Scripts use client-side separators (`DELIMITER //`, `GO`). |
| `parse_statement` | the shared adapter: `sqlparser` parses the statement whole | `sqlparser` cannot parse your `CREATE TABLE` whole. See [ClickHouse](reference.md#clickhouse-a-custom-ddl-adapter-and-a-binary-format). |
| `parse_type` | the shared adapter on a type string | The catalog spells types in a way the DDL parser does not read. |
| `quote_ident` | ANSI `"name"`, `"` doubled | Backticks (MySQL) or brackets (SQL Server). |
| `table_semantics` | nothing implied | Table engines imply keys or fixed column values. |
| `is_storage_table` | every table stores rows | Some `CREATE TABLE`s make views, proxies or integrations. |
| `enforces_foreign_keys` | `true` | Inserts do not check foreign keys. Then tables load in any order. |

## 4. Check the DDL side, without a server

Port the shop schema to your dialect as `tests/fixtures/shop.example.sql`, starting from the closest existing
one (`shop.postgres.sql`, `shop.mysql.sql` or `shop.sqlite.sql`). Then write
`tests/fixtures/<domain>/example.sql` for every domain directory (`library`, `hr`, `iot`, `blog`,
`banking`...), with the same table and column names as the other dialects. [Testing](testing.md#fixtures)
explains the rules.

Print what dbgen understood:

```sh
scripts/cargo-docker.sh run -- inspect --dialect example tests/fixtures/shop.example.sql
```

Every column should have the type you expect, and no column should be `Unsupported` unless it really cannot be
generated. Write the test file now (step 6) and run the offline test. It compiles and plans every fixture:

```sh
scripts/cargo-docker.sh test --test example fixtures_compile
```

## 5. The `Backend` half

Eleven methods are required. Write them in this order, because each one is needed to test the next.

1. **Connect and classify errors.** Build the driver's options from the URL. Keep a `Pool` of connections
   whose `open` closure also sets the session up: UTC time zone, ISO dates, foreign key checks on, the string
   escaping your `TextStyle` assumes. Write `classify(driver error) -> DbError` using the server's error codes
   (see [the guarantees](../CONNECTORS.md#the-guarantees-core-relies-on)). Every method maps errors through it.
2. **`describe`, `state_key`, `version`.** `describe` names the target for messages, without a password.
   `state_key` names the resume state file: build it from host, port and database with `file_safe`. `version`
   is the first call that reaches the server. A missing database must fail here with `UnknownDatabase`.
3. **`create_database`, `execute`, `query_rows`.** `dbgen apply` uses them. `create_database` connects without
   the database (or to an administrative one) and creates it if it is missing.
4. **`live_columns`.** One catalog query returning `(table, column, type)` rows, through `group_columns`. Include
   generated columns: core checks that every DDL column exists.
5. **`encode_row` and `insert`.** These are the load. A text format can use the toolkit: `sql_row` with your
   `TextStyle` and `insert_statements` for multi-row `INSERT`s (as MySQL and SQLite do), or `copy_field` for
   PostgreSQL's `COPY` text format. A binary format encodes from the column's `DataType` (as ClickHouse does).
   `insert` must be atomic: one statement, or a transaction around several.
6. **`truncate`.** Empties tables even when foreign keys point at them: truncate them in one statement, or turn
   the checks off around it.

The defaults of the rest are portable SQL. Override them only when your SQL differs:

| Method | Default | Override when | Example |
|---|---|---|---|
| `finish_load` | nothing | Sequences must move past the loaded keys, or statistics need a refresh. | PostgreSQL `setval` + `ANALYZE` |
| `max_writers` | no limit | The database has a single writer. | SQLite: 1 |
| `strict_types` | `false` | The bulk format is binary, so live types must equal the DDL's. | ClickHouse |
| `table_ref` | the quoted name | Queries need `database.table`. | ClickHouse |
| `cast_to_text` | `CAST(x AS TEXT)` | The database casts to text otherwise. | MySQL: `CAST(x AS CHAR)` |
| `count`, `read_rows` | `SELECT COUNT(*)`, `SELECT DISTINCT ... ORDER BY ... LIMIT` | Reads need a modifier, or another format is faster. | ClickHouse: `FINAL`, `RowBinary` |
| `duplicates_sql`, `orphans_sql`, `violations_sql`, `collapsed_sql` | portable SQL; no collapsed rows | Verification needs other SQL. | ClickHouse |
| `introspect_ddl` | an error: the connector cannot read its schema | Always worth it: users can then run without DDL files. | all built-in connectors |

`introspect_ddl` can come last, in a later pull request. Return the database's DDL in your dialect, written
back in its declared form where the catalog prints it differently. See
[schema introspection](reference.md#schema-introspection).

## 6. The test file

`tests/example.rs` says how to reach a server, and runs the whole conformance suite with one macro call:

```rust,ignore
//! Conformance tests of the ExampleDB connector, against a real server.
//!
//! Set `DBGEN_TEST_EXAMPLE_URL` to the server without a database, e.g.
//! `example://dbgen:dbgen@localhost:15999`. Without it only the offline tests run.

mod common;

use common::Server;
use dbgen::connectors::example::Example;
use dbgen::db::Backend;

fn server() -> Option<Server> {
    let base = match std::env::var("DBGEN_TEST_EXAMPLE_URL") {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => {
            eprintln!("DBGEN_TEST_EXAMPLE_URL not set: skipping");
            return None;
        }
    };
    let admin = format!("{base}/admin");
    Some(Server {
        connector: "example",
        url: Box::new(move |db| format!("{base}/{db}")),
        drop: Box::new(move |db| Example::connect(&admin).unwrap().execute(&format!("DROP DATABASE IF EXISTS {db}")).unwrap()),
        unknown_database: true,
        atomic_batches: true,
        introspection: true,
    })
}

conformance_tests! {
    connector: "example",
    server: server,
    prefix: "ex",
    resume_delete: "DELETE FROM orders WHERE order_id % 7 = 0",
}
```

- `prefix` keeps database names apart from other connectors' (`dbgen_it_ex_shop_test`).
- `resume_delete` deletes some order lines, directly or through `ON DELETE CASCADE`. The resume scenario checks
  that they come back.
- `unknown_database: false` opts out of the missing-database scenario, for a database with no such notion
  (SQLite creates the file). `atomic_batches: false` opts out of the atomicity scenario, for a database that
  does not enforce primary keys (ClickHouse). `introspection: false` opts out of the introspection round trip
  while `introspect_ddl` is not implemented. Opt out only for those reasons.

## 7. Run the suite

Add a service for your server to `docker-compose.yml` ([testing](testing.md#adding-a-test-server)), then:

```sh
docker compose up -d example
DBGEN_TEST_EXAMPLE_URL=example://dbgen:dbgen@localhost:15999 scripts/cargo-docker.sh test --test example
```

`scripts/cargo-docker.sh` passes the `DBGEN_TEST_*` variables it knows into the container. Add yours to it.
[Testing](testing.md) explains each scenario and how to read its failure, and
[troubleshooting](troubleshooting.md) lists the usual causes.

## 8. Finish

- `target/clippy.sh` (or `cargo clippy --all-targets`) is clean, and so is the build with only your feature:
  `cargo clippy --no-default-features --features example --lib --bins --test example -- -D warnings`.
- `cargo doc --no-deps --document-private-items` with `RUSTDOCFLAGS="-D warnings"` is clean.
- Go through the [pull request checklist](maintaining.md#pull-request-checklist).
