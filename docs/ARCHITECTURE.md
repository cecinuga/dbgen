# Architecture

The generator has to fill a warehouse with up to tens of millions of rows so that it stays
consistent across 31 tables, is reproducible, and survives interruption, without holding those
rows in memory. Two decisions make that possible, and most of the code follows from them.

## Decision 1: rows are pure functions

Row `i` of table `T` is a function of the seed, the row targets, the time window and `i`.
Nothing else: no previous row, no database read, no shared mutable state.

```
value = f(seed, stream tag, entity or row index, field)
```

- **Consistency without coordination.** `fact_alarms` and `fact_device_vitals_compact` never see each
  other, yet agree on who was in bed 17 at 03:12, because both ask the same function.
- **Flat memory.** Facts look dimensions up by computing them, so no key maps are loaded.
- **Reproducibility.** Output does not depend on `batch_size`, `workers`, thread scheduling or resume
  points. An integration test compares table digests across different batchings.
- **Safe retries.** A batch can be regenerated identically at any time.

The RNG is implemented in `rng.rs` rather than taken from `rand`, so a dependency upgrade cannot
silently change every dataset. Anything that changes the rows for a given seed must bump
`MODEL_VERSION` in `plan.rs`, which invalidates resume states.

## Decision 2: the schema is read, the semantics are written

| Derived from the live catalog (`schema.rs`) | Written by hand (`world/`, `tables/`, `vocab.rs`) |
|---|---|
| Column types and widths for the COPY layout | What a column means and which values it takes |
| Generated columns (excluded from COPY) | Which version, patient or staff member a fact refers to |
| Primary keys (resume probes) | Lifecycles and distributions |
| Foreign keys (load order) | Semantic prerequisites of a target |
| Serial sequences to advance | |
| Hypertables and their columnstore policies | |
| Seeded domains and SLA policy (data, read at startup) | |

A migration that widens a column, or adds a foreign key or a nullable column, needs no code change.
A migration that adds a `NOT NULL` column without a default, or removes a modelled column, fails
validation with a message naming it. The generator cannot invent the meaning of a new column.

## Components

```
config.yaml ─> config ─> dsn ─> db (pool)
                                 │
                schema (catalog) ┤
                                 ▼
           plan: validate ─ build World ─ dependency levels ─ fingerprint
                                 │
                                 ▼
    engine: work queue ─> generator threads ─> bounded queue ─> COPY workers ─> DWH
              (tables/ project World into rows, copy encodes them)
                                 │
                                 ▼
    run: sequences ─ policies ─ compression ─ ANALYZE ─ exact counts ─ report ─ verify
```

| Module | Responsibility |
|---|---|
| `config`, `dsn` | Parse and validate inputs; libpq-compatible `sslmode` handling |
| `schema`, `db` | Catalog introspection; maintenance statements; error classification |
| `rng`, `timeline`, `timeutil`, `vocab` | Deterministic primitives and domain vocabularies |
| `world/` | The simulated facility: hierarchy, people, shifts, devices, occupancy, alarms |
| `tables/` | One projection of the world per table; no table knows about another |
| `plan` | Everything that can fail before the first write; dependency levels |
| `engine` | The load pipeline: backpressure, retries, resume, cancellation |
| `copy` | PostgreSQL binary COPY encoding, including `numeric` |
| `state`, `run`, `progress`, `report`, `verify` | Orchestration and output |

## The world model

Row targets are independent inputs, so the model sizes itself from them.

**Timelines** (`timeline.rs`). A timeline splits a window into `n` contiguous intervals. Any boundary
can be computed alone, and "which interval contains t" is answered in O(1). Boundaries move by less
than half the shortest interval, so they can never cross. Every SCD2 chain, status history and
assignment history is a timeline, which is how a fact finds the version in force at its instant.

**Spreads** distribute `total` rows over `entities`, differing by at most one. Two layouts are used:
entity-major for SCD2 dimensions (so a surrogate key is `offset(entity) + version + 1`), and
time-major for facts (so a batch lands in a narrow range of chunks).

**Occupancy** (`world/occupancy.rs`) is the backbone. Stays happen in rounds; slot `j` is always the
same bed and its patient rotates as `(j + round) mod patients`. Since a round's stays stay inside it
and the rotation is injective, no bed holds two patients and no patient is in two beds, by
construction. Stays of the last round are open, and they are the current occupancy.

**Shifts** (`world/people.rs`). Staff work five days in seven, staggered, on one of three shifts. An
alarm can only be handled by someone on shift at that instant, so staff activity is coherent with
alarm handling.

**Alarms** (`world/alarms.rs`). Trigger times follow a diurnal profile and are monotonic in the row
number, so one day's alarms are a contiguous index range. That is what lets `fact_staff_activity`
aggregate exact daily counts per staff member the way the ETL does, one batch at a time. The
lifecycle is cut at the dataset's "now": an acknowledgment or resolution that would fall after the
window's end has not happened yet, which is where open alarms come from.

### Sizing rules

| Input | Derived |
|---|---|
| `dim_patient` rows | patients = rows / 1.2 (the rest are later SCD2 versions) |
| `dim_staff` rows | staff = rows / 1.1 |
| `dim_device` rows | radars and cameras, in proportion to beds and rooms |
| `dim_location`, `dim_room_version`, `dim_floor_version` rows | one version per entity, the extra rows distributed as later versions |
| `fact_patient_occupancy` rows | occupied beds = min(97% of beds, patients); rounds = stays / occupied beds |
| Any fact on a timeline | one interval or reading per `window / count(entity)` |

Rows per day are therefore a consequence of the target and the window. A large target on a small
facility gives valid but dense histories: `fact_floor_status: 3000000` on 4 floors means a status
change about every 42 seconds. Validation rejects only what is impossible or absurd (spacing under a
second, stays under five minutes). To keep densities realistic, scale the entities with the facts.

### What is exact and what is statistical

Some relations hold row by row, and `verify` checks them. Others can only hold in distribution,
because the row counts are independent inputs.

| Exact | Statistical |
|---|---|
| A fact's SCD2 key is the version in force at its instant | Number of bed-status changes vs number of stays |
| A vital reading's or bed alarm's patient occupies that bed at that instant | Historical bed status vs historical occupancy at a given instant |
| Current bed status is `Occupied` exactly when the bed has an open stay | Posture movements happening inside an active posture assignment |
| Current snapshots agree across grains and with layer 1 | Blackouts suppressing alarms: alarms are not filtered by blackouts |
| Staff activity counts equal the alarms handled, per day and staff member | |
| `dim_device.operational_status` is the last status reported | |

## Load order

Dependency levels come from Kahn's algorithm over live foreign keys plus `logical_deps()`, the
references that cannot be declared (`fact_media_records` → `fact_alarms`). A level loads only after
the previous one has committed.

The current schema has no cycles. A cycle would be reported as a validation error naming the tables,
not broken automatically: the generator has no model that needs a two-pass load, and untested code
for a case that does not occur would be a liability. Self-references are ignored for ordering.

## The pipeline

```
work queue ──> generator threads ──> bounded queue ──> COPY workers ──> DWH
(table,batch)  encode binary COPY    (backpressure)     one connection each
```

- **Binary COPY, one contiguous buffer per batch.** Physical layouts come from the catalog; values
  that do not fit are sticky errors naming table, column and row.
- **Backpressure.** The queue holds `queue_depth` encoded batches. Generation is far faster than
  ingestion, so generators mostly wait on it, and memory is bounded by
  `(queue_depth + workers + generator_threads) × batch bytes`.
- **Atomic batches.** One `COPY` in one transaction: a batch is fully committed or absent.
- **`synchronous_commit = off`.** Safe here: a batch lost to a server crash is found missing on
  resume. Set `synchronous_commit: true` to measure with durable commits.
- **Constraints stay on.** Foreign keys, CHECKs, the hierarchy trigger and all indexes run on every
  row. Dropping them would be faster, but would modify the schema and give up the server checking
  the data. It also makes the load cost resemble the ETL's, which is relevant to a stress test.

### Failure handling

| Failure | Behaviour |
|---|---|
| Lost connection, deadlock, server restart, resource exhaustion | Retry with backoff, up to `max_retries`. Before retrying, probe whether the batch committed before the error, so an ambiguous commit is not inserted twice. |
| Constraint or data error | Deterministic, so never retried. New batches stop, in-flight ones finish, and the report names table, batch, row range, SQLSTATE and constraint. |
| First SIGINT or SIGTERM | Same graceful stop, then restore policies, write state, print a summary. Exit code 130. |
| Second SIGINT, or SIGKILL | Immediate exit; the server rolls back uncommitted batches. State says `running`, and the next run resumes. |

### Resume

The database, not the state file, says what is committed: each batch of a populated table is probed
by the primary key of its first row, generated on the fly. Since batches are atomic and rows are
deterministic, that one lookup is conclusive. The state file (one per database, under
`.dwh-generator/` next to the config) holds what the database cannot tell: the plan's fingerprint,
the resolved time window, the batch size, and the columnstore jobs paused for the load.

The fingerprint covers model and generator version, seed, window, batch size, targets and schema
version. If any changed, resuming would mix rows of two plans, so the run refuses and lists the
differences. Rows without a matching state are refused as well, and `--reset` is the explicit way out.

### Columnstore policies

Loading a year of history into hypertables whose policy compresses chunks older than a few days
would race the background job. The policies are paused for the load and always restored, on failure
and interruption too, and after a crash by the next run, which finds them in the state file.
`compress_after_load: true` runs them once at the end, giving a production-like chunk layout.

## Verification

`dwh-generator verify` covers what the server cannot enforce: undeclared references, interval overlaps
on hypertables, the README's reconciliation checks, SCD2 versions in force, and cross-table agreement.
Each check is a query counting violations. Checks on empty tables are skipped, and a violation in
migration-seeded data is a warning, since the generator does not write that data.

`coherence.vitals_patient_is_bed_occupant` covers only the last three days of readings, to bound its
cost. The checks otherwise scan whole tables, so on large datasets they take noticeable time and
decompress any compressed chunks they read.

## Known limitations

- **The hierarchy never changes.** Beds do not move between rooms, and a device stays at one target.
  The schema supports moves; the coherence trigger makes them awkward to generate historically, and no
  report in the BFF depends on them.
- **All floors have `area_type = 'Floor'`,** as in both production-derived DWHs. Common areas are not generated.
- **`PATIENT` and `FLOOR` device assignments are not generated,** as in production.
- **Staff activity rows exist only for working days.** A night-shift alarm triggered after midnight on
  the handler's day off counts toward no row, whereas the ETL would create one. Counts in existing rows
  remain exact.
- **With fewer than three staff members,** shifts cannot all be covered by their own group, so
  someone handles alarms outside their nominal shift.
- **Version counts are even across entities** (differing by at most one), which is more uniform than
  production's skew.
- **`etl_metadata` is not updated,** so the BFF's freshness indicator keeps its seeded value, and a real ETL
  pointed at a generated DWH would attempt a full load.
- **Client-certificate authentication** (`sslcert`, `sslkey`) is not supported and is rejected explicitly.
