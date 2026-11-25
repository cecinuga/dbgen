# dbgen

Fills a database with synthetic data, so whatever reads it can be stress-tested at progressively larger volumes.
dbgen reads the schema from the SQL DDL you already have, such as a directory of migrations, or from the database
itself, so it needs no code for your tables. You set the number of rows per table, in a YAML or JSON file or on
the command line, and dbgen loads exactly that many. Keys are unique, references resolve and CHECK constraints
hold, and the same seed always gives the same data.

| Database | URL | Bulk loading |
|---|---|---|
| PostgreSQL | `postgres://user:password@host:5432/database` | `COPY ... FROM STDIN`, one per batch |
| MySQL, MariaDB | `mysql://user:password@host:3306/database` | multi-row `INSERT`s, one transaction per batch |
| SQLite | `sqlite:path/to/file.db` | multi-row `INSERT`s, one transaction per batch |
| ClickHouse | `http://user:password@host:8123/database` | `INSERT ... FORMAT RowBinary` over HTTP, one per batch |

It uses no production data. It never changes the schema, except when you run `dbgen apply`, which runs your DDL.

The main use is the Agevola reporting DWH on ClickHouse ([examples/agevola/](examples/agevola/)).
[examples/shop/](examples/shop/) has the same small shop schema for each database.

## Quick start

No Rust toolchain is needed on the host: the wrapper script builds inside the official Rust image. SQLite needs
no server, so it is the shortest way to try dbgen.

```bash
scripts/cargo-docker.sh build --release                              # binary at bin/dbgen

bin/dbgen inspect  -c examples/shop/sqlite.yaml                      # schema and plan, no database needed
bin/dbgen apply    -c examples/shop/sqlite.yaml --create-database    # create the database and run the DDL
bin/dbgen validate -c examples/shop/sqlite.yaml                      # check everything, write nothing
bin/dbgen generate -c examples/shop/sqlite.yaml --verify             # load, then check the result
```

For the other databases, `docker compose up -d` starts PostgreSQL on port 15432, MySQL on 13306 and ClickHouse on
18123, and `examples/shop/postgres.yaml`, `mysql.yaml` and `clickhouse.yaml` point at them.

A configuration file is optional. Against a database that already has its schema, flags are enough, and the
schema is read from the database:

```bash
bin/dbgen generate -d postgres://dbgen:dbgen@localhost:15432/shop --default-rows 1000 -r countries=existing --verify
bin/dbgen init     -d postgres://dbgen:dbgen@localhost:15432/shop -o dbgen.yaml     # or start a configuration
```

Other ways to build:

- With a local toolchain (Rust 1.85 or later), `cargo build --release` puts the binary at `target/release/dbgen`.
  SQLite is compiled in, and nothing else needs a system library.
- `docker build -t dbgen .` builds a self-contained image. Run it with
  `docker run --rm --network host -v "$PWD:/work" -w /work dbgen generate -c dbgen.yaml`. The configuration's
  `schema` paths must be inside the mounted directory.

## Commands

| Command | What it does | Database |
|---|---|---|
| `inspect` | Prints the schema, and the generation plan when there are row targets. | Only to read the schema from it |
| `init` | Writes a starting configuration that lists every table of the schema. | Only to read the schema from it |
| `validate` | Checks the settings against the schema and the live database, generates sample rows, and prints the plan. Writes nothing. | Read |
| `apply` | Runs every statement of the DDL files, in order. | Written |
| `generate` | Loads the rows. Resumes automatically if a previous run was interrupted. | Written |
| `verify` | Checks the data in the database: row counts, unique keys, references and CHECK constraints. | Read |

Every command reads its settings from a configuration file and from flags; a flag overrides the file's setting.
`dbgen <command> --help` groups them:

| Group | Flags | Setting |
|---|---|---|
| Target | `-c, --config FILE` | The configuration file, YAML or JSON. Default: `dbgen.yaml` in the current directory, if it exists. |
| | `-d, --database URL` (or `DBGEN_DATABASE`) | `database` |
| | `--dialect NAME` | `dialect` |
| | `-s, --schema PATH`, repeatable | `schema`. Without any, the schema is read from the database. |
| Data | `-r, --rows TABLE=N`, repeatable or comma-separated; `N` can also be `existing` or `skip` | one entry of `tables` |
| | `--default-rows N` | `generation.default_rows` |
| | `--seed N`, `--history-days N`, `--end-time WHEN`, `--null-rate R` | the `generation` key of the same name |
| | `--strict-tables` | `generation.missing_tables: error` |
| Load (`generate`) | `--batch-size N`, `-w, --workers N`, `--table-concurrency N`, `--max-retries N`, `--state-file FILE` | the `generation` key of the same name |
| Run | `--reset` (`generate`) | Empties every generated table first. **Destroys the data in those tables.** |
| | `--dry-run` (`generate`) | Validates and prints the plan, then stops. |
| | `--verify` (`generate`) | Runs the verification checks after a successful load. |
| | `--report FILE` (`generate`, `verify`) | Writes the summary, or the check results, as JSON. |
| | `--no-progress` (`generate`) | Prints periodic log lines instead of progress bars. |
| | `--create-database` (`apply`) | Creates the database first if it does not exist. |
| | `--json` (`inspect`) | Prints the schema as JSON. |
| | `--rows N`, `-o FILE`, `--force` (`init`) | Rows to propose per table; where to write; overwrite. |

Paths given as flags are relative to the current directory; paths in a configuration file are relative to the
file. `inspect` also takes DDL files as arguments, like `--schema`.

Global options: `--log-level LEVEL` (or `DBGEN_LOG`), which also accepts a filter such as `dbgen=debug,info`,
and `--log-format pretty|json`. Logs go to stderr. The plan, the summary and the check results go to stdout.

`apply` runs the statements exactly as written. On a database that already has the schema, it succeeds only if
the statements can run twice, as `CREATE TABLE IF NOT EXISTS` can.

## Configuration

Each file in [examples/shop/](examples/shop/) is a short complete example, and
[examples/agevola/dbgen.yaml](examples/agevola/dbgen.yaml) a real one. The file is YAML; JSON is valid YAML, so a
`.json` file works too. Every key is optional, and flags can replace the whole file.

```yaml
database: "postgres://dbgen:dbgen@localhost:15432/shop"
schema: migrations/

generation:
  seed: 7
  history_days: 90
  end_time: "2026-01-01"

tables:
  customers: 2000
  orders: 20000
  order_lines: 50000
  countries: existing

columns:
  orders.channel: { values: [store, web, app], weights: [2, 5, 3] }
```

| Key | Meaning |
|---|---|
| `database` | Where to load. See below. |
| `dialect` | `postgres`, `mysql`, `sqlite` or `clickhouse`. Taken from the `database` URL when omitted. |
| `schema` | DDL files or directories, one path or a list. Without it, the schema is read from the database. |
| `generation` | Seed, time window and load tuning. |
| `tables` | Rows per table. |
| `relations`, `ignore_relations` | References between tables that the DDL does not declare. |
| `columns` | Value settings for individual columns. |

Relative paths are resolved against the configuration file's directory. Unknown keys are errors, so a typo
cannot go unnoticed.

### `database`

The URL scheme selects the database, and with it the SQL dialect of the DDL.

| Database | URL forms | Notes |
|---|---|---|
| PostgreSQL | `postgres://` or `postgresql://user:password@host:port/database` | libpq's `key=value` form works too, with `dialect: postgres`. Tables are those of the current schema. |
| MySQL | `mysql://` or `mariadb://user:password@host:port/database` | |
| SQLite | `sqlite:path`, `sqlite://path` or `sqlite:///absolute/path` | A relative path is relative to the configuration file. |
| ClickHouse | `http://` or `https://user:password@host:port/database`, or `clickhouse://` | The HTTP interface only: the native ports 9000 and 9440 are refused. `https://`, `?secure=true` or port 8443 use TLS. |

dbgen connects to PostgreSQL and MySQL without TLS. A PostgreSQL URL with `sslmode=require` or stricter is refused.

`${VAR}` is expanded from the environment, which keeps passwords out of the file. An unset variable is an error.

```yaml
database: "postgres://app:${PG_PASSWORD}@db.internal:5432/reporting"
```

A configuration without `database` can still be inspected if it sets `dialect`.

### `schema`

The statements are applied in order, as a migration tool would apply them. A directory contributes its `*.sql`
files sorted by name. If the directory holds `*.up.sql` files, only those are used.

dbgen follows the statements that shape tables: `CREATE TABLE` (including `LIKE` and `AS other_table`), `ALTER
TABLE` column and constraint changes, `CREATE UNIQUE INDEX`, `DROP` and `RENAME` of tables and indexes, and
PostgreSQL's `CREATE TYPE ... AS ENUM` and `CREATE DOMAIN`. From these it learns the column types, defaults,
primary and unique keys, foreign keys, CHECK constraints and, on ClickHouse, the table engine and partitioning.
Two other statements change how a table is treated:

- an `INSERT` marks its table as seeded reference data;
- on ClickHouse, `CREATE MATERIALIZED VIEW ... TO target` marks the target as written by the view.

Every other statement, such as functions, triggers, views, grants or comments, is counted and ignored without
being parsed. `inspect` lists them.

#### Without `schema`: the schema of the database

When neither `schema` nor `--schema` gives DDL files, dbgen asks the database for its DDL and compiles it the same
way, so everything above applies. `dbgen inspect -d URL` shows what it found.

| Database | Where the schema comes from |
|---|---|
| PostgreSQL | Rebuilt from the catalogs of the current schema: enums, domains, tables with their columns, defaults, identity and generated columns, keys and CHECKs, unique indexes, then foreign keys. Partitions are left out; a partitioned table keeps its `PARTITION BY`. |
| MySQL | `SHOW CREATE TABLE` of every base table, parents first. |
| SQLite | The `sql` of `sqlite_schema`, for tables and indexes. |
| ClickHouse | `system.tables`, tables first, then views and materialized views, so the targets of views are still left alone. |

Each connector's tests load every fixture, read its schema back from the database, and check that dbgen derives
the same schema, the same plan and the same data from it as from the DDL files.

The database cannot say which tables hold reference data that migrations inserted, since that is not part of the
DDL. List those tables as `existing` (`-r countries=existing`), or `--default-rows` will fill them too.
`apply` needs DDL files, since it runs them.

### `generation`

| Key | Default | Meaning |
|---|---|---|
| `seed` | `42` | The same seed, DDL, configuration and window give identical data. |
| `history_days` | `365` | Length of the time window that dates and timestamps fall in. |
| `end_time` | 00:00 UTC today | End of the window, as `YYYY-MM-DD` or a timestamp. |
| `null_rate` | `0.1` | Share of NULLs in nullable columns. |
| `infer_relations` | `true` | Infer references from column names. See [Relations](#relations). |
| `fill_defaults` | `false` | Generate columns that have a `DEFAULT`, instead of leaving them to the database. |
| `missing_tables` | `warn` | Tables absent from `tables`: `warn` leaves them alone, `error` refuses to run. |
| `default_rows` | none | Rows for every table absent from `tables`, instead of leaving it alone. |
| `max_repair_steps` | `200` | Search steps per row to satisfy CHECK constraints. |
| `max_existing_rows` | `1000000` | Rows read from each table that is referenced but not generated. |
| `batch_size` | `100000` | Rows per batch. Each batch is inserted atomically. Does not change the data. |
| `workers` | `4` | Concurrent writing connections. Does not change the data. SQLite always uses one. |
| `table_concurrency` | `2` | Tables of one dependency level loaded at the same time. |
| `generator_threads` | `min(workers, CPUs)` | Threads generating and encoding rows. |
| `queue_depth` | `2 × workers` | Encoded batches waiting for a connection. Bounds memory use. |
| `max_retries` | `5` | Attempts per batch, on transient failures only. |
| `http_timeout_secs` | `300` | Timeout of one request to ClickHouse. |
| `state_file` | see [Resuming](#resuming) | Location of the resume state. |
| `progress_interval_secs` | `10` | Interval between progress log lines. |

Set `end_time` if you need identical data on different days. Without it, the window ends at midnight of the
day the run starts, so the same seed produces different timestamps tomorrow.

### `tables`

| Value | Treatment |
|---|---|
| a number | Generate exactly that many rows. |
| `existing` | Never written. Its rows are read when generated tables reference it. |
| `skip`, `0` or empty | Never written or read. |

Tables not listed are never written:

- Tables the DDL seeds with `INSERT`, and targets of ClickHouse materialized views, are treated as `existing`.
- ClickHouse tables that store no rows, such as `Null` and `Distributed` tables, are skipped.
- Any other table gets `default_rows` rows when it is set. Otherwise it is treated as `existing`, with a warning,
  or an error if `missing_tables` is `error`.

`dbgen init` writes a configuration that lists every table, as a starting point.

### Relations

A relation says that a column of a child table holds a key of a parent table. Each child row then points at an
existing parent row. dbgen finds relations in three places. When two sources claim the same column, the first
one in this list wins:

1. `relations` in the configuration.
2. `FOREIGN KEY` and `REFERENCES` declarations in the DDL. A reference without columns points at the parent's
   primary key.
3. Column names, when `infer_relations` is on. A column ending in `_id`, `_key`, `_uuid` or `_code` that names a
   table refers to that table. The table name may carry a `dim_` or `fact_` prefix and be singular or plural. The
   parent column is the one with the same name, else `id`, `key`, etc., else the parent's primary key. Leading
   words of the column name are a role: `acknowledger_staff_key` refers to `dim_staff.staff_key`.

`dbgen inspect -c FILE` lists every relation with its source (`config`, `ddl` or `inferred`). On PostgreSQL, MySQL
and SQLite, declared foreign keys usually cover everything. On ClickHouse, which has no foreign keys, declare the
references whose names do not reveal them, and remove wrong ones with `ignore_relations`:

```yaml
relations:
  - "orders.customer_id -> customers.customer_id"
  - "fact_alarms.(sla_category, sla_priority) -> dim_alarm_sla_target.(category_name, priority_name)"
  - { from: "fact_alarms.patient_key", to: "dim_patient.patient_key", distribution: skewed, skew: 2 }
ignore_relations: ["orders.parent_id"]
```

`distribution` is `uniform` (the default) or `skewed`. With `skewed`, a few parents get most children, and a
higher `skew` (default `1.5`) concentrates them more.

The parent can be a generated table or an `existing` one, whose rows dbgen reads first. If the parent has no
rows, a nullable child column is left NULL and a non-nullable one fails validation.

A table that references itself, such as `categories.parent_id`, gets a hierarchy without cycles: each row points
at an earlier row, and the first row is a root (NULL), unless the column cannot be NULL.

### `columns`

Settings for one column, under `table.column`:

| Setting | Effect |
|---|---|
| `values: [...]`, with optional `weights: [...]` | Picks from the list, proportionally to the weights. |
| `range: [lo, hi]` | Inclusive bounds: numbers, dates or timestamps. |
| `value: v` | Always `v`, which may be `null`. |
| `null_rate: 0.3` | Share of NULLs, for nullable columns. |
| `kind: email` | Kind of text: `first_name`, `last_name`, `full_name`, `email`, `phone`, `city`, `country`, `address`, `postal_code`, `url`, `sentence`, `word`, `code`, `uuid`, `json`, `ip`. |
| `unique: true` | Every value distinct. Fails validation if the type cannot hold that many values. |
| `generate: false` | Leaves the column to the database: its default, or NULL. |
| `after: col`, `gap: [min, max]` | A date or timestamp that follows `col` by a gap such as `[10s, 30m]`. Units: `us`, `ms`, `s`, `m`, `h`, `d`, `w`. |
| `length: [min, max]` | Number of elements of an array or map. |

### Columns without settings

- **Text** gets plausible values from the column name: `email` gets e-mail addresses, `city` city names,
  `*_status` or `*_type` a small set of codes, `*_name` labels such as `Floor 3`, and so on.
- **Numbers** get a range suggested by the last word of the name, such as a count or a duration.
- **Dates and timestamps** fall in the time window. Birth dates are in the past. On ClickHouse, the column a table
  is partitioned by follows row order, so each batch touches few partitions.
- **Enums** (PostgreSQL types, MySQL `ENUM`, ClickHouse `Enum8`) take their declared values.
- **Keys are unique by construction.** This covers primary keys, `UNIQUE` constraints and indexes,
  identifier-looking columns (`id`, `*_id`, `*_key`, `*_uuid`), and on ClickHouse the sorting key of engines that
  merge rows sharing it (`ReplacingMergeTree` and the like). For a composite key, dbgen enumerates combinations
  without repeats, and validation fails if more rows are requested than combinations exist.
- **Serial, identity and auto-increment keys** are generated like any other key, so that child rows can compute
  them. After the load, dbgen moves PostgreSQL sequences past the loaded values. MySQL and SQLite do this by
  themselves.
- **Other columns with a `DEFAULT`**, such as `created_at DEFAULT now()`, are left to the database, unless a key,
  a reference or a CHECK involves them. Generated (computed) columns are never inserted. ClickHouse engine control
  columns get the value that keeps the row: `is_deleted = 0`, `sign = 1`.
- **CHECK constraints** shape the values. `c = expr` computes `c`, `IN (...)` and comparisons narrow a column's
  values, and `end > start` makes `end` follow `start`. A row that still violates a check is repaired by a short
  search. `validate` generates sample rows of every table to catch impossible checks before anything is written.

## What the data guarantees

Every generated row satisfies the schema: keys are unique, references point at existing parent rows, and CHECK
constraints hold. On PostgreSQL, MySQL and SQLite, the database itself checks every foreign key during the load.
Row `i` of a table is a function of the seed and `i` only. That is why the data does not depend on `batch_size`,
`workers` or interruptions, and why a child row can compute the key of the parent row it references without
reading it.

dbgen knows the structure, not the domain. In the Agevola DWH, an alarm's `patient_key` is a valid patient, but
that patient is not necessarily in that bed at that time. Reports that join facts on domain rules will see
coherent keys and arbitrary combinations.

## How each database is loaded

| | PostgreSQL | MySQL | SQLite | ClickHouse |
|---|---|---|---|---|
| Order | Parents first | Parents first | Parents first | Any |
| Writers | `workers` | `workers` | One | `workers` |
| Session settings | `TIME ZONE 'UTC'` | `time_zone '+00:00'` | `foreign_keys = ON` | |
| After the load | Sequences moved past the keys, `ANALYZE` | `ANALYZE TABLE` | `ANALYZE` | |
| Live column types | Must exist | Must exist | Must exist | Must match the DDL exactly |

Where the database checks foreign keys, tables load by dependency level: a level starts once the previous one
has committed. A table that references itself loads one batch at a time, in row order, so every row it points at
is already there.

Every batch is inserted atomically, so after a failure a batch is either entirely in the table or absent.

## Agevola stress series

The Agevola reporting DWH runs on ClickHouse.

| Configuration | Target |
|---|---|
| [examples/agevola/dbgen.yaml](examples/agevola/dbgen.yaml) | `agevola_dwh` on the local ClickHouse of `docker compose` |
| [examples/agevola/reporting-test.yaml](examples/agevola/reporting-test.yaml) | The REPORTING-TEST ClickHouse on `localhost:8125`, password in `CH_PASSWORD` |
| [examples/agevola/stress/](examples/agevola/stress/) `stress-1.yaml` to `stress-6.yaml` | `agevola_stress_1` to `_6`, so the datasets can coexist |

They read the DDL from the ETL repository, and expect it next to this repository, at
`../reporting-etl-service/sql/migrations`.

The stress configurations share tables, relations and column settings, and differ only in the size of the fact
tables:

| Configuration | Rows per fact table | Total rows |
|---|---|---|
| `stress-1` | 50,000 | 750,927 |
| `stress-2` | 150,000 | 2,250,927 |
| `stress-3` | 500,000 | 7,500,927 |
| `stress-4` | 1,500,000 | 22,500,927 |
| `stress-5` | 5,000,000 | 75,000,927 |
| `stress-6` | 3,000,000 | 45,000,927 |

Each total is 15 fact tables times the per-table figure, plus 927 rows of dimensions and `fact_vitals_daily`.
Note that `stress-5` is larger than `stress-6`.

To run a series, applying the DDL to each database and keeping one report per configuration:

```bash
scripts/run-stress-suite.sh examples/agevola/stress/*.yaml
scripts/run-stress-suite.sh --no-verify examples/agevola/stress/stress-5.yaml
```

The script works with any configuration, on any database. It **empties the generated tables of each target
database** before loading it, and stops at the first failure. Reports and logs go to `reports/`. `BIN` and
`REPORTS` override the binary and the output directory.

## Monitoring a run

On a terminal, `generate` shows one progress bar per active table plus a total. Otherwise, as under CI or
`nohup`, or with `--no-progress`, it logs rows done, percentage, rate and ETA at each interval.

The summary at the end looks like this:

```text
Table                              Target Rows   Inserted Rows    Final Rows   Repaired  Duration      Rows/s Retries  Status
----------------------------------------------------------------------------------------------------------------------
orders                                  20,000          20,000        20,000      1,862  00:00:00     219,301       0  Success
order_lines                             50,000   20,000+30,000        50,000          0  00:00:00     251,844       0  Success
```

| Column | Meaning |
|---|---|
| Inserted Rows | Rows written by this run. `20,000+30,000` means 30,000 were already committed by an interrupted run. |
| Final Rows | An exact `COUNT(*)` taken after the load, not a counter kept by dbgen. |
| Repaired | Rows that needed the CHECK repair search. |
| Rows/s | Rows inserted divided by the table's elapsed time. Tables loading at the same time share the workers, so use the total for throughput. |
| Retries | Batches retried after a transient failure. |
| Status | `Success` (final equals target), `Mismatch` (load finished, counts differ), `Incomplete` (run stopped first). |

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Load or verification failed |
| 2 | Invalid configuration, DDL or plan |
| 3 | Cannot connect |
| 130 | Interrupted; can be resumed |

### Verification

`verify`, or `generate --verify`, runs one query per check, each counting violations:

| Check | What it counts |
|---|---|
| `rows.TABLE` | Difference between the row count and the target. |
| `unique.TABLE.(KEY)` | Duplicate keys, for every primary key, unique key and, on ClickHouse, merging sorting key. |
| `collapse.TABLE` | ClickHouse: rows lost to background merges, on engines that merge rows. |
| `check.TABLE.NAME` | Rows violating a CHECK constraint. |
| `reference.CHILD.COLS -> PARENT.COLS` | Non-NULL references without a parent row, for every relation. |

`PASS` means zero violations, and `SKIP` means the child table of a reference is empty. `FAIL` and `ERROR` make
the command exit with 1. The checks scan whole tables, so on large datasets they take noticeable time.

## Resuming

Run the same command again. Committed batches are kept, and only the rest are inserted.

- **Ctrl-C once** stops taking new batches, lets in-flight inserts finish, saves the state and prints a summary.
  **Twice** exits immediately.
- The state lives in `.dbgen/<target>.json` next to the configuration, one file per database. `<target>` is
  `<host>_<port>_<database>`, or `sqlite_<file name>`. `generation.state_file` overrides it.
- A generated table whose row count differs from what the state records is emptied and loaded again, with a
  warning. Where the database checks foreign keys, the tables that reference it are reloaded too.

dbgen refuses to resume if the seed, batch size, window, row targets, DDL, relations or column settings
changed, and lists what differs. Without a state file, it refuses to write to generated tables that already hold
rows. In both cases, the way out is to restore the configuration or use `--reset`.

Transient failures, such as deadlocks, lock timeouts, lost connections, or ClickHouse's "too many parts" and
memory limits, are retried up to `max_retries` times, waiting from 1 to 32 seconds. If a request fails after the
server stored its rows, the retry duplicates them. The final count then reports a `Mismatch`, and the next run
reloads that table.

## Testing

```bash
scripts/cargo-docker.sh test --lib            # unit tests, no database
scripts/cargo-docker.sh test --test sqlite    # SQLite conformance tests, no server

docker compose up -d
DBGEN_TEST_POSTGRES_URL=postgres://dbgen:dbgen@localhost:15432 \
DBGEN_TEST_MYSQL_URL=mysql://root:dbgen@localhost:13306 \
DBGEN_TEST_CLICKHOUSE_URL=http://dbgen:dbgen@localhost:18123 \
  scripts/cargo-docker.sh test
```

Every connector runs the same conformance suite ([tests/common/mod.rs](tests/common/mod.rs)):

- the fixtures of [tests/fixtures/](tests/fixtures/) compile and plan, without a server;
- every domain there (library, hr, iot, blog, banking) loads and passes verification;
- the shop schema loads, gives identical data for different batch sizes and worker counts, and resumes after rows
  are deleted;
- a batch that fails leaves nothing behind, and a missing database is reported as such.

The original ClickHouse tests also check the row encoding against the server. The tests create, fill and drop
databases named `dbgen_it_*_test`, and touch nothing else. Without a server's variable, only its offline tests
run. The unit tests need the `clickhouse` feature, which the default build has.

## Performance

Indications, not a benchmark: one workstation (12 threads, 15 GB), each database in a local Docker container with
its default settings, the shop examples (about 72,000 rows each) with the default 4 workers.

| Database | Throughput |
|---|---|
| ClickHouse | about 400,000 rows/s |
| SQLite | about 170,000 rows/s |
| PostgreSQL | about 73,000 rows/s |
| MySQL | about 23,000 rows/s |

These loads are too short to show steady-state rates, and the databases check every constraint and index on every
row. On ClickHouse, the `stress-1` volume (750,927 rows) loaded at about 820,000 rows/s
(`reports/reporting-test.json`). Larger volumes have not been measured yet.

Memory use is bounded by `(queue_depth + workers + generator_threads) × size of one encoded batch`.

## Known limitations

- **Structure, not domain.** See [What the data guarantees](#what-the-data-guarantees).
- **No TLS to PostgreSQL and MySQL.** Use a local tunnel to reach a server that requires it.
- **CHECK constraints using functions dbgen does not evaluate** are reported by `validate` and left to the
  database. If the database then rejects a row, the load fails.
- **Types dbgen cannot generate**, such as PostgreSQL `interval`, `time`, ranges and geometric types, or ClickHouse
  aggregate function states, are left NULL when nullable or to their default when they have one. Otherwise
  validation fails.
- **SQLite stores decimals as floating point**, so a CHECK that needs exact decimal arithmetic, such as
  `a + b + c = 1`, can fail there even when dbgen's values satisfy it.
- **Keys with expression parts**, such as a unique index on `lower(email)` or `ORDER BY toDate(ts)`, are not
  guaranteed unique. `validate` warns about them.
- **Statement splitting** happens at `;`. Bodies with inner `;` work inside dollar quotes (PostgreSQL) and in
  `CREATE TRIGGER ... BEGIN ... END`, but not after a MySQL client `DELIMITER` command.
- **Referenced tables that are not generated** are read up to `max_existing_rows` rows each.
- **A schema read from the database** cannot tell which tables migrations seeded (list them as `existing`). On
  PostgreSQL and MySQL, primary-key columns read back as `NOT NULL` even when the DDL did not say so, and MySQL
  reports `TINYINT(1)` as a boolean.
- **CHECKs on PostgreSQL domains** are not evaluated by dbgen, from DDL files or from the database. A column whose
  domain restricts its values may need settings under `columns`.

## Building with fewer databases

Each connector is a Cargo feature named after it, all on by default. A build without a connector does not
compile its driver and cannot reach that database:

```bash
cargo build --release --no-default-features --features postgres,sqlite
```

## Adding a database

A database is supported by a connector: one module under [src/connectors/](src/connectors/), registered with one
line, that tells dbgen how the database spells its DDL and how to connect, read its catalog and load rows. The
rest of dbgen is the same for every database. [docs/CONNECTORS.md](docs/CONNECTORS.md) explains how to write one,
test it against the shared fixtures and submit it, and how connectors are maintained.

## Source

Each module starts with a description of its part, and [src/lib.rs](src/lib.rs) shows how they fit together.
