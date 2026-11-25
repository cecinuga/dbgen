# Test fixtures

Schemas that the integration tests load into every supported database. Each one is a realistic schema written the
way its database is usually written, so the tests exercise each dialect's own syntax rather than a common
subset.

## Layout

| Path | Used by |
|---|---|
| `shop.sql`, `shop.yaml` | The original ClickHouse tests (`tests/clickhouse.rs`) |
| `shop.<dialect>.sql` | The shop scenarios of each SQL database: load, determinism, resume |
| `<domain>/<dialect>.sql` | The DDL of a domain in one dialect |
| `<domain>/config.yaml` | Row targets and settings shared by every dialect of the domain |

`config.yaml` holds only `generation`, `tables`, `relations` and `columns`. A test prepends `database` and
`schema`, so one configuration serves every dialect, and column names are the same in each.

## Domains

| Domain | Rows | What it exercises |
|---|---|---|
| `library` | 20,550 | Seeded lookup table, many-to-many link with a composite key, dates that follow each other, enums and domains |
| `hr` | 28,070 | Two self-referencing hierarchies, a CHECK between two numeric columns, a history keyed by (employee, date), generated columns |
| `iot` | 55,535 | Time-series readings keyed by (device, instant), UUID and IP columns, a skewed relation, codecs, TTL and a materialized view on ClickHouse |
| `blog` | 38,100 | Threaded comments (nullable self-reference), a CHECK tying status to a date, JSON and arrays, triggers with multi-statement bodies |
| `banking` | 58,040 | Decimals, two references to the same table (account and counterparty), sign/direction CHECKs, a partitioned table on PostgreSQL |

Each dialect also brings its own idioms: PostgreSQL enums, domains, identity and serial keys, `PARTITION OF`;
MySQL `AUTO_INCREMENT`, `ENUM`, `MODIFY`/`CHANGE COLUMN`; SQLite `WITHOUT ROWID`, `STRICT`, untyped columns;
ClickHouse engines, `Map`/`Array`/`JSON` columns, `MATERIALIZED`/`ALIAS` columns and materialized views.
Functions, views, triggers and comments appear too, since dbgen must skip them.

## Adding a dialect or a domain

- **A new database:** write `<domain>/<dialect>.sql` for every domain, plus `shop.<dialect>.sql`, keeping the
  column names of the other dialects. The connector guide in `docs/` explains how the tests pick them up.
- **A new domain:** create `<domain>/config.yaml` and one `.sql` per dialect. When the column names do not reveal
  a reference (`manager_id`, `author_id`), declare it under `relations`, since ClickHouse has no foreign keys.

A fixture must load with `dbgen apply --create-database` followed by `dbgen generate --verify`. When `validate`
rejects a schema because a CHECK cannot be satisfied, constrain the columns in `config.yaml`, as
`hr/config.yaml` does for `max_salary > min_salary`, rather than weakening the schema.
