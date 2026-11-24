# dwh-generator

Fills the Agevola reporting DWH with synthetic data, so the reporting system can be stress-tested at
progressively larger volumes. You set the number of rows per table in a YAML file; the generator loads
exactly that many, consistent across tables and reproducible from a seed.

It uses no production data and needs no pre-populated tables. It runs against the schema created by the
`reporting-etl-service` migrations and never changes that schema.

## Quick start

No Rust toolchain is needed on the host; the wrapper script builds inside the official Rust image.

```bash
docker compose up -d                                # TimescaleDB on localhost:5436
scripts/cargo-docker.sh build --release             # binary at bin/dwh-generator
scripts/provision-dwh.sh config.yaml                # create the database and apply the ETL migrations

bin/dwh-generator validate --config config.yaml     # check config and schema, print the plan, write nothing
bin/dwh-generator generate --config config.yaml --verify
```

With a local toolchain (Rust 1.85 or later), `cargo build --release` works too, and the binary is at
`target/release/dwh-generator`.

`provision-dwh.sh` expects the ETL repository at `../reporting-etl-service`. Set `MIGRATIONS_DIR` if it
is elsewhere.

## Commands

| Command | What it does |
|---|---|
| `validate --config FILE` | Checks the config against the live schema and prints the plan. Writes nothing. |
| `generate --config FILE` | Loads the configured rows, and resumes automatically if a previous run was interrupted. |
| `verify --config FILE` | Runs the integrity and coherence checks on what is in the database. |

Options for `generate`:

| Option | Effect |
|---|---|
| `--reset` | Truncates every generated table first. Seeded reference data is kept. **Destroys existing generated data.** |
| `--dry-run` | Validates and prints the plan, then stops. |
| `--verify` | Runs the verification checks after a successful load. |
| `--report FILE` | Writes the summary as JSON. `verify` accepts it too. |
| `--no-progress` | Prints periodic log lines instead of progress bars. |

Global options: `--log-level LEVEL` (or `DWHGEN_LOG`) and `--log-format pretty|json`.

## Configuration

`config.yaml` is a working example.

```yaml
DWH_DB_SOURCE: "postgresql://postgres:postgres@localhost:5436/reporting_dwh_db?sslmode=disable"

generation:
  seed: 42
  batch_size: 10000
  workers: 8

tables:
  dim_floor: 6
  dim_room: 46
  fact_alarms: 50000
  # ...
```

### `DWH_DB_SOURCE`

A PostgreSQL connection string, as a URL or in `key=value` form, and the only connection setting. All
`sslmode` values are supported, along with `sslrootcert`. Client certificates are not.

`${VAR}` is expanded from the environment, which keeps passwords out of the file:

```yaml
DWH_DB_SOURCE: "postgresql://dwh:${DWH_PASSWORD}@db:5432/reporting_dwh_db"
```

### `generation`

| Key | Default | Meaning |
|---|---|---|
| `seed` | `42` | Same seed, targets and window give identical data. |
| `batch_size` | `10000` | Rows per `COPY`. Each batch commits atomically. Does not change the data. |
| `workers` | `8` | Concurrent `COPY` connections. Does not change the data. |
| `table_concurrency` | `1` | Tables of one dependency level loaded at the same time. |
| `generator_threads` | `min(workers, CPUs)` | Threads encoding batches. |
| `queue_depth` | `2 × workers` | Encoded batches waiting for a connection. Bounds memory use. |
| `history_days` | `365` | Length of the simulated history. |
| `end_time` | 00:00 UTC today | End of the history, as `YYYY-MM-DD` or RFC 3339. |
| `max_retries` | `5` | Attempts per batch, on transient failures only. |
| `missing_tables` | `warn` | Generated tables absent from `tables`: `warn` leaves them empty, `error` refuses to run. |
| `pause_columnstore_policies` | `true` | Pauses TimescaleDB compression during the load and restores it afterwards. |
| `compress_after_load` | `false` | Runs the compression policies once after the load. |
| `analyze_after_load` | `true` | Runs `ANALYZE` on the loaded tables. |
| `synchronous_commit` | `false` | `synchronous_commit` for loader sessions. |
| `state_file` | see [Resuming](#resuming) | Location of the resume state. |
| `progress_interval_secs` | `10` | Interval between progress log lines. |

Set `end_time` if you need identical data on different days. Without it, the history ends at midnight of
the day the run starts, so the same seed produces different timestamps tomorrow.

### `tables`

Each value is the exact number of rows the generator inserts into that table.

| Tables | Treatment |
|---|---|
| Seeded: `dim_area_type`, `dim_bed_status`, `dim_room_status`, `dim_floor_status`, `dim_alarm_priority`, `dim_alarm_category`, `dim_alarm_sla_target`, `languages`, `translation` | Never written. Omit them, set 0, or state their current size. |
| Operational: `etl_metadata`, `etl_load_checkpoint` | Never written. Omit them or set 0. |
| All other tables | Generated. See `config.yaml` for the full list. |

Some targets depend on each other, and `validate` reports every violation in one pass:

- `dim_location` ≥ `dim_bed`, `dim_room_version` ≥ `dim_room`, `dim_floor_version` ≥ `dim_floor`. Every
  entity needs at least one version; extra rows become later versions.
- `fact_staff_activity` ≤ 5 × staff × weeks of history, because the table holds one row per staff member
  per working day. Staff is about `dim_staff / 1.1`.
- Facts need their parents: for example `fact_patient_occupancy` needs `dim_patient` and `dim_location`.
- Histories cannot be denser than one event per second per entity, and stays cannot be shorter than five minutes.

`dim_patient` and `dim_staff` targets count SCD2 rows, not people: 60 `dim_patient` rows is 50 patients.

## Stress-test series

`configs/config.1.yaml` to `config.6.yaml` target one database each, `reporting_dwh_db_1` to `_6`, so the
datasets can coexist. They are corrected copies of the `config.N.yaml` files in the project root, which do
not pass validation. Fact volumes are unchanged, and a header in each file lists its changes.

| Config | Rows per fact table | Total rows |
|---|---|---|
| 1 | 50,000 | 800,565 |
| 2 | 150,000 | 2,400,988 |
| 3 | 500,000 | 8,002,469 |
| 4 | 1,500,000 | 24,006,700 |
| 5 | 5,000,000 | 80,021,507 |
| 6 | 3,000,000 | 48,013,046 |

Two things to be aware of:

- Config 5 is larger than config 6. This comes from the original files.
- These configs keep a 60-bed facility, so `dim_staff` grows to satisfy `fact_staff_activity`, reaching 12,693
  rows in config 6. To keep a realistic staff size, lower `fact_staff_activity` instead.

To run a series, provisioning each database and keeping one report per config:

```bash
scripts/run-stress-suite.sh                                       # all of configs/
scripts/run-stress-suite.sh configs/config.1.yaml configs/config.2.yaml
scripts/run-stress-suite.sh --no-verify configs/config.5.yaml
```

The script **resets each target database** before loading it, and stops at the first failure. Reports and
logs go to `reports/`.

To point the reporting system at a dataset, set its DWH connection to the matching database.

## Monitoring a run

On a terminal, `generate` shows one progress bar per active table plus a total. Otherwise, as under CI or
`nohup`, or with `--no-progress`, it logs rows done, percentage, rate and ETA at each interval.

The summary at the end looks like this:

```text
Table                             Target Rows  Inserted Rows     Final Rows   Duration       Rows/s  Retries  Status
--------------------------------------------------------------------------------------------------------------
fact_alarms                           500,000        500,000        500,000   00:06:13        1,341       25  Success
fact_patient_events                   500,000 460,000+40,000        500,000   00:06:03        1,267        0  Success
```

| Column | Meaning |
|---|---|
| Inserted Rows | Rows written by this run. `460,000+40,000` means 40,000 were already committed by an interrupted run. |
| Final Rows | An exact `count(*)` taken after the load, not a counter kept by the generator. |
| Rows/s | Rows inserted divided by the table's elapsed time. When tables load concurrently they share the workers, so use the total for throughput. |
| Retries | Batches retried after a transient failure such as a deadlock. Retried batches are not duplicated. |
| Status | `Success` (final equals target), `Mismatch` (load finished, counts differ), `Incomplete` (run stopped first). |

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Load or verification failed |
| 2 | Invalid configuration or schema |
| 3 | Cannot connect |
| 130 | Interrupted; can be resumed |

### Verification

`verify` runs about 60 queries that each count violations of something the database cannot enforce:

- references that hypertables cannot declare
- overlapping intervals
- facts pointing at an SCD2 version not in force at their instant
- the reconciliation checks in `sql/README.md` of the ETL repository
- agreement between tables, such as a vital reading's patient occupying that bed at that instant

`Pass` means zero violations, and `Skip` means a table involved is empty. `Fail` and `Error` make the command exit
with 1.

On a freshly migrated DWH, expect one `Warn`: `reconciliation.6.priority_weights_sum`. The weights seeded in
`dim_alarm_priority` sum to 1.8, which comes from the migrations and not from generated data. See
[docs/SCHEMA.md](docs/SCHEMA.md#findings-in-the-existing-schema).

## Resuming

Run the same command again. Each batch commits atomically, so committed batches are detected and kept, and
only the rest are inserted.

- **Ctrl-C once** stops new batches, lets in-flight ones commit, restores the compression policies and prints a
  summary. **Twice** exits immediately; the server rolls back whatever had not committed.
- The state lives in `.dwh-generator/<host>_<port>_<database>.json` next to the config, one file per database.
- If a run dies abruptly, the compression policies stay paused until the next run of that config restores them.

The generator refuses to resume if the seed, targets, window, batch size or schema version changed, and lists what
differs. It also refuses to write to generated tables that hold rows it cannot account for. In both cases, the way
out is to restore the config or use `--reset`.

This means applying a new migration to a partly loaded database prevents resuming it.

## Testing

```bash
scripts/cargo-docker.sh test --lib      # unit tests, no database

scripts/provision-dwh.sh reporting_dwh_db_test
DWH_TEST_DB_SOURCE='postgresql://postgres:postgres@dwh:5432/reporting_dwh_db_test?sslmode=disable' \
  scripts/cargo-docker.sh test --test integration -- --test-threads=1
```

Integration tests truncate their database, so they refuse any database whose name does not end in `_test`. Without
`DWH_TEST_DB_SOURCE` they are skipped and report as passed.

To measure encoding speed without a database:

```bash
scripts/cargo-docker.sh test --release --lib bench_encode -- --ignored --nocapture
```

## Performance

Measured on one workstation, with PostgreSQL inside a Docker Desktop VM limited to 3.6 GB of RAM. These are
indications rather than a benchmark. All of them predate the current default of `table_concurrency: 1`, which has
not been measured yet.

| Run | Rows | Throughput |
|---|---|---|
| Encoding only, no database, one thread | | 0.7M rows/s (`fact_alarms`) to 12M rows/s (`fact_bed_status`) |
| Full load, tables of a level interleaved | 2.28M | about 40,000 rows/s |
| Level 5 of a resumed load, 2 tables at once | 1M | about 57,000 rows/s combined |
| Level 4 of the same resumed load, 8 tables at once, integration tests running part of the time | 4.24M | about 12,000 rows/s, 30 deadlocks retried |

PostgreSQL is the bottleneck, not generation. Most of the contention observed comes from TimescaleDB creating chunks:
each new chunk takes locks on its hypertable and on the dimensions it references, so hypertables loading at the same
time block each other and sometimes deadlock. Deadlocks are retried, but cost time.

Disk use was about 1.1 GB for the 8M-row dataset before compression. By extrapolation, not measurement, configs 5 and
6 would need roughly 11 GB and 7 GB.

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how generation stays consistent and reproducible, the load pipeline,
  what is exact and what is only statistical, known limitations.
- [docs/SCHEMA.md](docs/SCHEMA.md): the DWH schema, the rules that live in ETL and BFF code, findings in the existing schema.
