# Maintaining connectors

For maintainers: how core changes reach connectors, what to check when a connector changes, and the checklists
for pull requests.

## Who owns what

Core owns everything outside `src/connectors/`: the configuration, planning, generation, the engine, resume,
verification, reports, and the toolkit (`db::pool`, `db::text`, `dialect::standard`, `sql::convert`). A
connector owns its module, its fixtures, its test file and its Cargo feature. A change to core must keep every
registered connector compiling and passing the conformance suite, in the same pull request.

## Changes that break connectors

The connector API is everything a connector imports. These changes are **breaking**:

- a changed or removed method of `Connector` or `Backend`, or a new required one;
- a changed contract, even with the same signature. For example: `insert` may now receive empty batches, or
  `live_columns` must now include views;
- a change to `RowRef`, `Batch`, `CellError`, `DbError`/`DbErrorKind`, `LiveColumns` or `TextRows`;
- a new `DataType` or `Value` variant. Every `encode_row` must handle it, and the binary encoders match on
  them exhaustively;
- a changed field of `TypeRules`, a changed `Ddl`/`AlterAction`/`TableDef` (custom DDL adapters build them),
  or a changed signature or output of a toolkit function (`copy_field`, `sql_literal`, `insert_statements`...).

These changes are **not breaking**: a new `Connector` or `Backend` method with a default that keeps today's
behavior, a new toolkit function, and a new optional field read through a default.

A breaking change:

1. updates every in-tree connector, `src/connectors/template.rs` and this guide in the same pull request;
2. says "connector API" in the pull request title, and describes the change and how to migrate in its body;
3. adds a line to the [log](#connector-api-changes) at the end of this page, so authors of connectors in open
   pull requests know what to change.

## Which changes change the data

Three things decide what ends up in the database, and each is guarded differently:

| What changes | Example | Guard |
|---|---|---|
| How core generates rows from the plan | the RNG, a generator, the repair of CHECKs | Bump `MODEL_VERSION` in `src/plan.rs`. It enters every plan fingerprint, so interrupted runs refuse to resume with the new rows. |
| What the DDL compiles to | a connector's `TypeRules`, `custom_type`, `parse_statement`, `table_semantics` | Nothing to bump. The fingerprint includes every column's type and every CHECK, so a different schema refuses to resume by itself. Say in the pull request that the schema compiled from existing DDL changes. |
| How a connector writes values it already received | `encode_row`, the `TextStyle`, session settings | Nothing to bump, and nothing guards it: a resumed run would mix old and new encodings. The change is a fix if the old encoding was wrong. Otherwise it should not exist. Say so in the pull request. |

When a change must not change the data, compare the old and the new binary as [testing](testing.md#how-determinism-is-checked)
describes.

## Upgrading a driver

- Read the driver's changelog for changes in type conversions, time zone handling, TLS defaults and error types.
  `classify` matches on error variants and codes, and an upgrade can reclassify errors without a compile error.
- Keep the dependency lean: `default-features = false` and only the features the connector uses. The built-in
  connectors avoid TLS stacks and C libraries, so dbgen builds with nothing but Cargo.
- Run the connector's conformance suite and the data comparison against the previous binary.
- Commit `Cargo.lock` with the upgrade.

## Cargo features

Each connector sits behind a feature named after it, which enables its driver through `dep:`. The registry
line declares the module under that feature, and the connector is absent from a build without it: its name,
schemes and `--dialect` value are unknown there. Check that a connector builds alone, and that no other code
depends on it:

```sh
cargo clippy --no-default-features --features example --lib --bins --test example -- -D warnings
cargo clippy --no-default-features -- -D warnings   # no connector at all
```

The library's own unit tests use the ClickHouse dialect, so `cargo test --lib` needs the `clickhouse` feature.
Each integration test file declares `required-features` in `Cargo.toml`.

## Keeping clippy and the docs clean

```sh
target/clippy.sh                                    # cargo clippy --all-targets, default features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items
cargo test --doc                                    # the Rust examples of this guide
```

Every module starts with a `//!` header saying what it is for. In a connector, the header says how DDL is read
and how rows are loaded. Every public item has a doc comment, and trait methods state their contract there,
not in this guide.

## Pull request checklist

For contributors adding or changing a connector:

- [ ] One module in `src/connectors/<name>/` with a `//!` header, one line in the registry, one feature in
      `Cargo.toml` with the driver as an optional `dep:`, and a `[[test]]` entry with `required-features`.
- [ ] `name`, `aliases` and `schemes` are lowercase, and claim nothing generic another database could need.
- [ ] Errors are classified by server error codes, with each `Transient` and `UnknownDatabase` code named in a
      comment.
- [ ] `insert` is atomic, `count` is exact, `live_columns` includes generated columns, and `read_rows` is
      deterministic.
- [ ] Every session setting the encoding relies on (time zone, escaping, foreign key checks) is set when a
      connection opens.
- [ ] `tests/<name>.rs` invokes `conformance_tests!`, with no scenario opted out except for the reasons in
      [testing](testing.md#the-conformance-suite).
- [ ] `tests/fixtures/shop.<name>.sql` and a `<name>.sql` in every domain directory, with a comment for anything
      the dialect cannot express.
- [ ] A `docker-compose.yml` service, and the `DBGEN_TEST_<NAME>_URL` variable passed in
      `scripts/cargo-docker.sh`.
- [ ] If the connector implements `introspect_ddl`, `introspection: true` and the round trip passes on every
      fixture. Otherwise `introspection: false`, and the pull request says so.
- [ ] The suite passes against the server, and the pull request says against which server version.
- [ ] Clippy is clean with default features and with only this feature; the docs build without warnings.

## Review checklist

For maintainers reviewing a connector pull request:

- [ ] Nothing outside `src/connectors/<name>/`, the registry line, `Cargo.toml`, `Cargo.lock`, the tests,
      fixtures, `docker-compose.yml` and `scripts/cargo-docker.sh` changed. A change to core is a separate
      discussion.
- [ ] The driver is maintained, light, licensed compatibly, and pulls no system library or TLS stack you do not
      want in every build.
- [ ] `classify`: deadlocks, lock timeouts and lost connections are `Transient`; constraint violations and
      syntax errors are `Failed`; the missing-database code is `UnknownDatabase`. Guessing from message text is
      a last resort.
- [ ] `encode_row` refuses what it cannot write exactly (`CellError`) rather than converting it.
- [ ] No scenario is opted out without the reason the suite allows. The suite ran against a real server, with
      the output in the pull request.
- [ ] `introspect_ddl`, if implemented, reads only the current database or schema, and every rewrite of a
      catalog form into its declared form says why in a comment.
- [ ] The fixtures keep table and column names identical to the other dialects, and the domain matrix runs all
      of them.
- [ ] The default build with every connector, and the build with only this one, are clippy-clean.

## Connector API changes

Newest first. Each entry: date, what changed, how to migrate.

- **2026-11-25**: `Backend::introspect_ddl` added, optional: by default an error saying the connector cannot
  read its schema. Implement it to let dbgen run without DDL files, and set `introspection: true` in the
  connector's tests.
- **2026-11-25**: connectors introduced. `Dialect` is a handle on a registered `Connector`, no longer an enum,
  and each database is a module of `src/connectors/` behind a Cargo feature. `Dialect::Clickhouse`,
  `Dialect::Postgres`, `Dialect::Mysql`, `Dialect::Sqlite`, `dbgen::db::clickhouse` and
  `dbgen::dialect::clickhouse` remain as compatibility paths.
