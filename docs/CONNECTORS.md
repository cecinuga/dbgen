# Connectors

A connector is the part of dbgen that works with one database. Everything else is the same for every
database: configuration, planning, row generation, the load engine, resume, verification and reporting. To
support a new database you write a connector, add one line to a registry, and open a pull request. Nothing in
the engine or elsewhere in core changes.

This guide is for two kinds of readers:

- **Contributors**: you know Rust and your database, but not dbgen's internals, and want to add a connector.
- **Maintainers**: you review connector pull requests and keep connectors working as core evolves.

| Page | Read it when |
|---|---|
| This page | First. What a connector is, and what core relies on it for. |
| [Tutorial](connectors/tutorial.md) | You are writing a connector: from the template to passing the conformance tests. |
| [Reference](connectors/reference.md) | You need the contract of a method, or how an existing connector solved something. |
| [Type mapping](connectors/types.md) | You are mapping your database's types, or a column fails to load. |
| [Testing](connectors/testing.md) | You are writing fixtures, running the suite, or adding a test server. |
| [Maintaining](connectors/maintaining.md) | You are changing core, upgrading a driver, or reviewing a pull request. |
| [Troubleshooting](connectors/troubleshooting.md) | Something fails, and you want to know how the existing connectors handled it. |

The contract of every trait method lives in the rustdoc of the code. Read it in
[`src/connector.rs`](../src/connector.rs) and [`src/db/mod.rs`](../src/db/mod.rs), or render it with
`cargo doc --no-deps --open` if you have a local toolchain. This guide explains the contracts and links to the
code. It does not repeat the rustdoc.

## What a connector is

A connector has two halves:

| Half | Trait | Exists | Knows |
|---|---|---|---|
| Static | [`Connector`](../src/connector.rs) | For the whole program, without a connection | Names and URL schemes, the SQL dialect of the DDL, type names, capabilities. It opens a `Backend`. |
| Runtime | [`Backend`](../src/db/mod.rs) | One per target database, holding its connections | The catalog, the bulk format, statements, counts. |

There are two halves because the DDL must compile without a database: `dbgen inspect` and planning have no
connection, and a `Backend` exists only once a URL is known.

Core reaches a connector only through a `Dialect`, a copyable handle on a registered `Connector`. The
configuration's `dialect:` key, the scheme of its `database:` URL, and `dbgen inspect --dialect` all resolve to
one through the registry in [`src/connectors/mod.rs`](../src/connectors/mod.rs):

```rust
use dbgen::dialect::Dialect;

// Every connector compiled into this build, by name, alias or URL scheme.
let pg = Dialect::from_name("pg").expect("built with the postgres feature");
assert_eq!(pg.name(), "postgres");
assert_eq!(Dialect::from_scheme("postgresql"), Some(pg));

// The handle dereferences to the connector.
assert_eq!(pg.product(), "PostgreSQL");
assert_eq!(pg.quote_ident("order"), "\"order\"");
assert!(pg.enforces_foreign_keys());
```

The code lives here:

| Path | What |
|---|---|
| `src/connector.rs` | The `Connector` trait, the `Dialect` handle, `TableSemantics`. |
| `src/db/mod.rs` | The `Backend` trait and its types (`Batch`, `RowRef`, `DbError`...), `Database`, and shared helpers. |
| `src/db/pool.rs`, `src/db/text.rs`, `src/dialect/standard.rs`, `src/sql/convert.rs` | The toolkit connectors share: a connection pool, text encodings, the DDL adapter, type conversion. |
| `src/connectors/mod.rs` | The registry: one line per connector. |
| `src/connectors/<name>/` | One connector. `template.rs` is a commented one to copy. |
| `tests/common/mod.rs` | The conformance suite every connector runs. |

## What a connector does, and does not do

A connector **does**:

- name itself, and claim URL schemes;
- turn its DDL into dialect-neutral commands, usually through the shared adapter, and know its type names;
- say what its table definitions imply (engines that merge rows, tables that store nothing) and whether it
  enforces foreign keys;
- connect, and classify its driver's errors;
- read the catalog: which tables and columns exist, and with which types;
- encode rows in its bulk format and insert them, one batch at a time;
- empty tables, count rows, and read the rows of reference tables;
- supply SQL for verification where portable SQL does not work, and do maintenance after a load (sequences,
  statistics);
- optionally, read the schema back from the database as DDL in its dialect (`introspect_ddl`), so dbgen can run
  without DDL files.

A connector **does not**:

- generate values, choose row counts, or decide which parent a row references. Rows come from core and are a
  pure function of the seed;
- order tables or retry batches. It declares whether foreign keys are enforced and classifies errors, and core
  decides;
- keep resume state, run verification checks or write reports. Core does, through the connector's primitives.

## The guarantees core relies on

These are the promises a connector makes. Retries, resume and verification are correct only if they hold. The
conformance suite tests each one it can.

1. **Values arrive unchanged.** Core generates the same rows for the same seed, whatever the batch size, worker
   count or interruptions. The connector must store exactly those values: no rounding, no time zone shift, no
   truncation. A value the column cannot hold exactly is an error (`CellError`), not an approximation.
2. **Each batch is atomic.** `Backend::insert` writes every row of the batch or none. Core retries a failed
   batch whole, and resume counts whole batches. Use one statement, one transaction or one request per batch.
3. **Counts are exact.** `Backend::count` returns the rows a fresh read sees, not an estimate. On resume, a
   count that differs from the state makes core empty and reload the table.
4. **Errors are classified correctly.** `DbErrorKind` decides what core does next:

   | Kind | Means | Core |
   |---|---|---|
   | `Failed` | Deterministic: retrying fails the same way (constraint violation, syntax, bad value). | Stops the load. |
   | `Transient` | Nothing was written, or the server was briefly unable to serve (deadlock, lock timeout, too many connections). | Retries the batch up to `max_retries` times, with a growing wait. |
   | `Unreachable` | No answer from the server. | Retries during a load. Fails when connecting (exit code 3). |
   | `UnknownDatabase` | The server is up, the database is missing. | Fails, suggesting `dbgen apply --create-database`. |

   A failure misclassified as transient retries a hopeless batch `max_retries` times. A transient failure
   misclassified as failed stops a load that would have succeeded.
5. **The catalog is complete.** `Backend::live_columns` lists every table and every column, including generated
   columns. Before loading, core checks the DDL against it.
6. **Reads are deterministic.** `Backend::read_rows` returns the same rows in the same order for the same data.
   Generated rows that reference existing tables depend on it.
7. **It is thread-safe.** One `Backend` serves the whole run and is shared by the writer threads (`Send + Sync`).
   `max_writers` caps how many insert at once.

## Next

Start with the [tutorial](connectors/tutorial.md). Before opening the pull request, go through the checklist in
[maintaining](connectors/maintaining.md#pull-request-checklist).
