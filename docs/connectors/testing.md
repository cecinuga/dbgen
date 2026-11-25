# Testing a connector

A connector is tested at three levels:

| Level | Where | Needs a server | What |
|---|---|---|---|
| Unit tests | `#[cfg(test)] mod tests` in the connector's module | no | URL parsing, type names, encoded bytes: what you can check without a server. |
| Registry tests | `src/connectors/mod.rs` | no | For every registered connector: names, aliases and schemes are unique and lowercase; they resolve back to the connector; serde and `--dialect` round-trip; quoted identifiers read back as one identifier. Your connector gets these automatically. |
| Conformance suite | `tests/common/mod.rs`, run by `tests/<name>.rs` | mostly | The guarantees core relies on, on real fixtures and a real server. |

## The conformance suite

`tests/<name>.rs` defines `server()` and invokes `conformance_tests!` (see the
[tutorial](tutorial.md#6-the-test-file)). The macro emits these tests:

| Test | Proves | When it fails, look at |
|---|---|---|
| `fixtures_compile` | The shop fixture and every domain fixture of the connector compile, and their configurations plan, without a database. | Type names (`custom_type`), statements `sqlparser` cannot parse, `TypeRules`. |
| `shop_end_to_end` | A load passes every verification check, holds the exact target rows, and the configured column domain and the CHECKs hold in the database. | `encode_row` (values changed on the way), `count`, the verification SQL. |
| `same_seed_same_data_for_any_batching` | Two loads with different batch sizes and worker counts store identical rows. | Encoding that depends on the batch (statement splitting, per-connection settings), time zones. |
| `resume_reloads_tables_that_lost_rows` | After rows are deleted, the next run sees the exact counts differ, reloads the tables, and stores identical rows again. | `count`, `truncate` with foreign keys, `enforces_foreign_keys`. |
| `batches_are_atomic` | A batch whose second row repeats the first's primary key fails without leaving a row, with a non-transient error, and the row alone then loads. | `insert` (one statement or one transaction), `classify`. |
| `missing_database_is_reported` | The first call to a missing database fails with `UnknownDatabase`. | `classify`, `version`. |
| `every_domain_end_to_end` | Every `tests/fixtures/<domain>/` with a `<name>.sql` applies, loads every table to its target, and passes verification. All failing domains are reported together. | The domain's fixture and the messages; then the rows above. |
| `introspection_round_trip` | The shop and every domain fixture, once applied, read back through `introspect_ddl` as the same schema: tables with their engine and materialized-view target, columns with types and nullability, keys, foreign keys, CHECK counts. Both schemas plan with the same warnings and the same repairs in a sample of 500 rows per table. A failure prints the differences and the introspected DDL. | `introspect_ddl`: a catalog form that compiles differently from the declaration (see [schema introspection](reference.md#schema-introspection)). |

Three scenarios can be turned off in `Server`, only for these reasons:

- `unknown_database: false`, for a database with no notion of a missing database (SQLite creates the file);
- `atomic_batches: false`, for a database that does not enforce primary keys, where a failed batch cannot be
  provoked (ClickHouse);
- `introspection: false`, for a connector that does not implement `introspect_ddl`, which is optional.

The suite prints a line when it skips one.

ClickHouse is the exception in layout. Its shop scenarios predate the suite and run on its own shop fixture in
`tests/clickhouse.rs`. `tests/clickhouse_conformance.rs` runs the rest of the suite, calling the scenario
functions one by one.

## Fixtures

### The shop

`tests/fixtures/shop.<name>.sql` is the shop schema in your dialect. Port the closest existing one. The suite
relies on these names:

- `countries` (`code`, `name`), seeded with an `INSERT` in the fixture; the configuration says `existing`;
- `categories` (`category_id` primary key, `parent_id` referencing `categories`);
- `customers`, `products`, `orders` (`order_id`, `channel` with a CHECK on `store`/`web`/`app`, `store_id`
  required exactly when `channel = 'store'`);
- `order_lines`, with a primary key on `(order_id, product_id)` and a foreign key to `orders`. Use
  `ON DELETE CASCADE` if the database has it.

Keep the CHECKs your database can express, and say in a comment at the top what has no equivalent. The SQLite
fixture, for instance, explains why it drops the exact-sum CHECK.

### Domains

Each directory `tests/fixtures/<domain>/` (`library`, `hr`, `iot`, `blog`, `banking`...) holds:

- `config.yaml`: `generation`, `tables`, `relations` and `columns` only. The suite prepends `database` and
  `schema`;
- `<connector>.sql`: the domain in each dialect.

Table and column names are the same in every dialect, so one configuration serves all of them. When you add a
connector, write a `<name>.sql` for every domain. The suite runs each one it finds, so the whole matrix comes for
free. A table that only exists in your dialect, such as an audit table filled by a trigger, is left alone, with
a warning, as long as the configuration does not name it.

To add a domain, create its directory with a `config.yaml` and one `.sql` per connector. Every connector picks
it up. Check it with the offline test first: `fixtures_compile` reports every fixture that does not compile.

## Running

```sh
# Offline: unit tests, registry tests, and fixtures_compile of every connector.
scripts/cargo-docker.sh test --lib
scripts/cargo-docker.sh test --test example fixtures_compile

# One connector against its server.
DBGEN_TEST_EXAMPLE_URL=example://dbgen:dbgen@localhost:15999 scripts/cargo-docker.sh test --test example

# Everything.
docker compose up -d
DBGEN_TEST_POSTGRES_URL=postgres://dbgen:dbgen@localhost:15432 \
DBGEN_TEST_MYSQL_URL=mysql://root:dbgen@localhost:13306 \
DBGEN_TEST_CLICKHOUSE_URL=http://dbgen:dbgen@localhost:18123 \
  scripts/cargo-docker.sh test
```

Without its variable, a connector's server tests pass without doing anything, and say so on stderr
(`--nocapture` shows it). The tests create, fill and drop databases named `dbgen_it_<prefix>_*_test`, and touch
nothing else. A failed test leaves its database in place for inspection, and the next run drops it first.

The whole suite takes about a minute on a laptop. MySQL is the slowest, at about 40 s.

## Adding a test server

Add a service to `docker-compose.yml`, published on `127.0.0.1` only, with a health check:

```yaml
  example:
    image: exampledb/server:4.2
    container_name: dbgen-example
    environment:
      EXAMPLE_USER: dbgen
      EXAMPLE_PASSWORD: dbgen
    ports:
      - "127.0.0.1:${DBGEN_EXAMPLE_PORT:-15999}:5999"
    healthcheck:
      test: ["CMD-SHELL", "example-cli ping"]
      interval: 5s
      timeout: 5s
      retries: 30
    restart: unless-stopped
```

Then add a row to the table in the file's header comment, and pass the variable into the container in
`scripts/cargo-docker.sh` (`-e DBGEN_TEST_EXAMPLE_URL`).

## How determinism is checked

Rows are a pure function of the seed, so the same configuration must store the same data, whatever the batch
size, worker count or interruptions, and whatever the connector's own choices.

- **Within the suite**, `same_seed_same_data_for_any_batching` loads the shop twice, with 5000 rows per batch on
  3 workers and with 1777 on 2. It then compares every column of every table, in key order. Columns the
  database fills (`DEFAULT now()`) are left out. `resume_reloads_tables_that_lost_rows` makes the same
  comparison before the deletion and after the reload.
- **Across versions**, when a change must not change the data (a refactoring, a driver upgrade, a core change),
  compare the old and the new binary:
  - `dbgen inspect -c <config> --json` must print byte-identical schemas for every fixture;
  - after `apply` and `generate` with each binary, every table must hold identical rows. Compare ordered dumps
    or per-table hashes, leaving out the columns the database fills.

  On ClickHouse, the raw rows of a materialized view's target depend on when background merges ran, so they
  differ even between two runs of the same binary. Leave such tables out, or compare them after `OPTIMIZE ...
  FINAL`.
- **With and without DDL files**, for a connector that implements `introspect_ddl`: load each fixture once from
  its DDL file and once without `schema`, so the schema is read from the database. Both loads must store
  identical rows. All built-in connectors pass this on every fixture.
