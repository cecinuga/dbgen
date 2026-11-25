# dbgen

Fills a ClickHouse database with synthetic data, so whatever reads it can be stress-tested at progressively
larger volumes. dbgen reads the schema from the SQL DDL you already have, such as a directory of migrations, so
it needs no code for your tables. You set the number of rows per table in a YAML file, and dbgen loads exactly
that many. Keys are unique, references resolve and CHECK constraints hold, and the same seed always gives the
same data.

It uses no production data. It never changes the schema, except when you run `dbgen apply`, which runs your DDL.

The main use is the Agevola reporting DWH ([examples/agevola/](examples/agevola/)). The integration tests run it
on [tests/fixtures/shop.sql](tests/fixtures/shop.sql), a small schema unrelated to Agevola.

## Quick start

No Rust toolchain is needed on the host: the wrapper script builds inside the official Rust image.

```bash
docker compose up -d                       # ClickHouse 26.8, HTTP on localhost:18123, user dbgen / dbgen
scripts/cargo-docker.sh build --release    # binary at bin/dbgen

bin/dbgen inspect  -c tests/fixtures/shop.yaml                     # schema and plan, no database needed
bin/dbgen apply    -c tests/fixtures/shop.yaml --create-database   # create the database and run the DDL
bin/dbgen validate -c tests/fixtures/shop.yaml                     # check everything, write nothing
bin/dbgen generate -c tests/fixtures/shop.yaml --verify            # load, then check the result
```

Other ways to build:

- With a local toolchain (Rust 1.85 or later), `cargo build --release` puts the binary at `target/release/dbgen`.
- `docker build -t dbgen .` builds a self-contained image. Run it with
  `docker run --rm --network host -v "$PWD:/work" -w /work dbgen generate -c dbgen.yaml`. The configuration's
  `schema` paths must be inside the mounted directory.

The Agevola configurations read the DDL from the ETL repository. They expect it next to this repository, at
`../reporting-etl-service/sql/migrations`.

## Commands

| Command | What it does | Database |
|---|---|---|
| `inspect [-c FILE] [FILES...]` | Compiles the DDL and prints the schema. With a configuration, also prints the generation plan. | Not needed |
| `validate -c FILE` | Checks the configuration against the DDL and the live database, generates sample rows, and prints the plan. Writes nothing. | Read |
| `apply -c FILE` | Runs every statement of the DDL, in order. | Written |
| `generate -c FILE` | Loads the configured rows. Resumes automatically if a previous run was interrupted. | Written |
| `verify -c FILE` | Checks the data in the database: row counts, unique keys, references and CHECK constraints. | Read |

`-c` (`--config`) defaults to `dbgen.yaml` in the current directory.

Options:

| Option | Command | Effect |
|---|---|---|
| `--reset` | `generate` | Empties every generated table first. **Destroys the data in those tables.** |
| `--dry-run` | `generate` | Validates and prints the plan, then stops. |
| `--verify` | `generate` | Runs the verification checks after a successful load. |
| `--report FILE` | `generate`, `verify` | Writes the summary, or the check results, as JSON. |
| `--no-progress` | `generate` | Prints periodic log lines instead of progress bars. |
| `--create-database` | `apply` | Creates the database first if it does not exist. |
| `--json` | `inspect` | Prints the compiled schema as JSON. |
| `--dialect NAME` | `inspect` | Dialect of `FILES` (default and only value: `clickhouse`). |

Global options: `--log-level LEVEL` (or `DBGEN_LOG`), which also accepts a filter such as `dbgen=debug,info`,
and `--log-format pretty|json`. Logs go to stderr. The plan, the summary and the check results go to stdout.

`apply` runs the statements exactly as written. On a database that already has the schema, it succeeds only if
the statements can run twice, as `CREATE TABLE IF NOT EXISTS` can.

## Configuration

[tests/fixtures/shop.yaml](tests/fixtures/shop.yaml) is a short complete example, and
[examples/agevola/dbgen.yaml](examples/agevola/dbgen.yaml) a real one.

```yaml
database: "http://dbgen:dbgen@localhost:18123/dbgen_shop_test"
schema: shop.sql

generation:
  seed: 7
  history_days: 90
  end_time: "2026-01-01"

tables:
  customers: 2000
  orders: 20000
  order_lines: 50000
  countries: existing

relations:
  - "customers.country_code -> countries.code"

columns:
  orders.status: { values: [new, paid, shipped, cancelled], weights: [1, 3, 5, 1] }
```

| Key | Meaning |
|---|---|
| `database` | Where to load. See below. |
| `dialect` | SQL dialect of the DDL. Only `clickhouse` exists, and it is the default. |
| `schema` | DDL files or directories, one path or a list. |
| `generation` | Seed, time window and load tuning. |
| `tables` | Rows per table. |
| `relations`, `ignore_relations` | References between tables that the DDL does not declare. |
| `columns` | Value settings for individual columns. |

Relative paths are resolved against the configuration file's directory. Unknown keys are errors, so a typo
cannot go unnoticed.

### `database`

`http://user:password@host:8123/database`. Use `https://`, or add `?secure=true`, for TLS, where the default port
is 8443. `clickhouse://` is accepted and means HTTP. The user and the database both default to `default`. dbgen
uses the HTTP interface only, and rejects the native protocol ports 9000 and 9440.

`${VAR}` is expanded from the environment, which keeps passwords out of the file. An unset variable is an error.

```yaml
database: "http://clickhouse:${CH_PASSWORD}@localhost:8125/reporting_dwh_db"
```

### `schema`

The statements are applied in order, as a migration tool would apply them. A directory contributes its `*.sql`
files sorted by name. If the directory holds `*.up.sql` files, only those are used.

dbgen follows `CREATE TABLE` (including `AS other_table`), `ALTER TABLE` column and constraint changes,
`MODIFY ORDER BY`, and `DROP`, `RENAME` and `EXCHANGE` of tables. From these it learns the column types,
defaults, keys, CHECK constraints, table engine and partitioning. Two other statements change how a table is
treated:

- an `INSERT` marks its table as seeded reference data;
- `CREATE MATERIALIZED VIEW ... TO target` marks the target as written by the view.

All other statements are counted and ignored. `inspect` lists them.

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
| `max_repair_steps` | `200` | Search steps per row to satisfy CHECK constraints. |
| `max_existing_rows` | `1000000` | Rows read from each table that is referenced but not generated. |
| `batch_size` | `100000` | Rows per `INSERT`. Does not change the data. |
| `workers` | `4` | Concurrent `INSERT` connections. Does not change the data. |
| `table_concurrency` | `2` | Tables loaded at the same time, parents first. |
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

- Tables the DDL seeds with `INSERT`, and targets of materialized views, are treated as `existing`.
- Tables that store no rows, such as views or `Null` and `Distributed` tables, are skipped.
- Any other table is treated as `existing`, with a warning, or an error if `missing_tables` is `error`.

Inserting into a table that a materialized view reads fires the view, as it would in production. In the shop
example, `daily_sales` fills up from `orders` and `order_lines`.

### Relations

A relation says that a column of a child table holds a key of a parent table. Each child row then points at an
existing parent row. dbgen finds relations in three places. When two sources claim the same column, the first
one in this list wins:

1. `relations` in the configuration.
2. `FOREIGN KEY` declarations in the DDL.
3. Column names, when `infer_relations` is on. A column ending in `_id`, `_key`, `_uuid` or `_code` that names a
   table refers to that table. The table name may carry a `dim_` or `fact_` prefix and be singular or plural. The
   parent column is the one with the same name, else `id`, `key`, etc., else the parent's primary key. Leading
   words of the column name are a role: `acknowledger_staff_key` refers to `dim_staff.staff_key`.

`dbgen inspect -c FILE` lists every relation with its source (`config`, `ddl` or `inferred`). Declare the ones
whose names do not reveal them, and remove wrong ones with `ignore_relations`:

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
| `length: [min, max]` | Number of elements of an `Array` or `Map`. |

### Columns without settings

- **Text** gets plausible values from the column name: `email` gets e-mail addresses, `city` city names,
  `*_status` or `*_type` a small set of codes, `*_name` labels such as `Floor 3`, and so on.
- **Numbers** get a range suggested by the last word of the name, such as a count or a duration.
- **Dates and timestamps** fall in the time window. Birth dates are in the past. The column a table is
  partitioned by follows row order, so each batch touches few partitions.
- **Keys are unique by construction.** This covers primary keys, `UNIQUE` keys, the sorting key of engines that
  merge rows sharing it (`ReplacingMergeTree`, `CollapsingMergeTree`, `SummingMergeTree` and the like), and
  identifier-looking columns (`id`, `*_id`, `*_key`, `*_uuid`). For a composite key, dbgen enumerates
  combinations without repeats, and validation fails if more rows are requested than combinations exist.
- **Columns with a `DEFAULT`** are left to the database, unless a key, a reference or a CHECK involves them.
  `MATERIALIZED` and `ALIAS` columns are never inserted. Engine control columns get the value that keeps the
  row: `is_deleted = 0` for `ReplacingMergeTree`, `sign = 1` for collapsing engines.
- **CHECK constraints** shape the values. `c = expr` computes `c`, `IN (...)` and comparisons narrow a column's
  values, and `end > start` makes `end` follow `start`. A row that still violates a check is repaired by a short
  search. `validate` generates sample rows of every table to catch impossible checks before anything is written.

## What the data guarantees

Every generated row satisfies the schema: keys are unique, references point at existing parent rows, and CHECK
constraints hold. Row `i` of a table is a function of the seed and `i` only. That is why the data does not depend
on `batch_size`, `workers` or interruptions, and why a child row can compute the key of the parent row it
references without reading it.

dbgen knows the structure, not the domain. In the Agevola DWH, an alarm's `patient_key` is a valid patient, but
that patient is not necessarily in that bed at that time. Reports that join facts on domain rules will see
coherent keys and arbitrary combinations.

## Agevola stress series

| Configuration | Target |
|---|---|
| [examples/agevola/dbgen.yaml](examples/agevola/dbgen.yaml) | `agevola_dwh` on the local ClickHouse of `docker compose` |
| [examples/agevola/reporting-test.yaml](examples/agevola/reporting-test.yaml) | The REPORTING-TEST ClickHouse on `localhost:8125`, password in `CH_PASSWORD` |
| [examples/agevola/stress/](examples/agevola/stress/) `stress-1.yaml` to `stress-6.yaml` | `agevola_stress_1` to `_6`, so the datasets can coexist |

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

The script **empties the generated tables of each target database** before loading it, and stops at the first
failure. Reports and logs go to `reports/`. `BIN` and `REPORTS` override the binary and the output directory.

To point the reporting system at a dataset, set its DWH connection to the matching database.

## Monitoring a run

On a terminal, `generate` shows one progress bar per active table plus a total. Otherwise, as under CI or
`nohup`, or with `--no-progress`, it logs rows done, percentage, rate and ETA at each interval.

The summary at the end looks like this:

```text
Table                              Target Rows   Inserted Rows    Final Rows   Repaired  Duration      Rows/s Retries  Status
----------------------------------------------------------------------------------------------------------------------
fact_alarms                             50,000          50,000        50,000          0  00:00:00     253,021       0  Success
fact_bed_status                        150,000  50,000+100,000       150,000          0  00:00:00     312,447       0  Success
```

| Column | Meaning |
|---|---|
| Inserted Rows | Rows written by this run. `50,000+100,000` means 100,000 were already committed by an interrupted run. |
| Final Rows | An exact `count()` taken after the load, not a counter kept by dbgen. |
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
| `unique.TABLE.(KEY)` | Duplicate keys, for every primary key, `UNIQUE` key and merging sorting key. |
| `collapse.TABLE` | Rows lost to background merges, on engines that merge rows. |
| `check.TABLE.NAME` | Rows violating a CHECK constraint. |
| `reference.CHILD.COLS -> PARENT.COLS` | Non-NULL references without a parent row, for every relation. |

`PASS` means zero violations, and `SKIP` means the child table of a reference is empty. `FAIL` and `ERROR` make
the command exit with 1. The checks scan whole tables, some with `FINAL`, so on large datasets they take
noticeable time.

## Resuming

Run the same command again. Committed batches are kept, and only the rest are inserted.

- **Ctrl-C once** stops taking new batches, lets in-flight inserts finish, saves the state and prints a summary.
  **Twice** exits immediately.
- The state lives in `.dbgen/<host>_<port>_<database>.json` next to the configuration, one file per database.
  `generation.state_file` overrides it.
- A generated table whose row count differs from what the state records is emptied and loaded again, with a
  warning.

dbgen refuses to resume if the seed, batch size, window, row targets, DDL, relations or column settings
changed, and lists what differs. Without a state file, it refuses to write to generated tables that already hold
rows. In both cases, the way out is to restore the configuration or use `--reset`.

Transient failures, such as timeouts, network errors, "too many parts" or memory limits, are retried up to
`max_retries` times, waiting from 1 to 32 seconds. If a request fails after the server stored its rows, the retry
duplicates them. The final count then reports a `Mismatch`, and the next run reloads that table.

## Testing

```bash
scripts/cargo-docker.sh test --lib      # unit tests, no database

docker compose up -d
DBGEN_TEST_CLICKHOUSE_URL=http://dbgen:dbgen@localhost:18123 scripts/cargo-docker.sh test --test clickhouse
```

The integration tests create, fill and drop databases named `dbgen_it_*_test`, and touch nothing else. They
check the row encoding against the server, load the shop schema end to end, compare data across different batch
sizes and worker counts, and resume after rows are lost. Without `DBGEN_TEST_CLICKHOUSE_URL` they are skipped and
report as passed.

## Performance

One measurement so far, in `reports/reporting-test.json`: the `stress-1` volume (750,927 rows, 61 MiB) loaded
into ClickHouse 26.8 in Docker with 4 workers and batches of 100,000 rows. The load took 0.9 seconds, about
820,000 rows per second, and the whole command 2.1 seconds. Larger volumes have not been measured yet.

Memory use is bounded by `(queue_depth + workers + generator_threads) × size of one encoded batch`.

## Known limitations

- **ClickHouse only.** The SQL front end and the database layer are written to take more dialects, but none other
  exists yet.
- **Structure, not domain.** See [What the data guarantees](#what-the-data-guarantees).
- **CHECK constraints using functions dbgen does not evaluate** are reported by `validate` and left to the
  database. If the database then rejects a row, the load fails.
- **Types dbgen cannot generate**, such as aggregate function states, are left NULL when nullable or to their
  default when they have one. Otherwise validation fails.
- **Keys with expression parts**, such as `ORDER BY toDate(ts)`, are not guaranteed unique. `validate` warns about
  them.
- **Referenced tables that are not generated** are read up to `max_existing_rows` rows each.

## Source

Each module starts with a description of its part. [src/lib.rs](src/lib.rs) shows how they fit together.
