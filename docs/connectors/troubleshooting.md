# Troubleshooting

Common failures when writing a connector, and how the built-in connectors solved them.

## Statement splitting

**Symptom**: `fixtures_compile` or `dbgen apply` fails in the middle of a function or trigger body, or a
statement is cut at a `;` that is not its end.

The default `Connector::split` tokenizes the whole file with the connector's `sqlparser` dialect and splits at
top-level `;`. Tokenizing handles quoted strings and dollar-quoted bodies (PostgreSQL `$$ ... $$`). The
`BEGIN ... END` body of a `CREATE TRIGGER` stays whole, which covers SQLite and MySQL triggers. What it cannot
handle is a client-side separator, such as MySQL's `DELIMITER //` or SQL Server's `GO`. A connector whose
scripts use one overrides `split`.

Statements that cannot change table structure (functions, grants, views, data changes) are never parsed. The
shared adapter counts them as ignored. So unusual syntax in them only matters to splitting, not to parsing.

## Catalog type mismatches

**Symptom**: `validate` or `generate` fails with "column T.C is in the DDL but not in the database", or "is X
in the DDL but Y in the database".

- *Not in the database*: `live_columns` misses columns. The SQLite connector used `pragma_table_info`, which
  leaves out generated columns, and the `hr` fixture's `GENERATED ALWAYS AS (...) VIRTUAL` column failed. It
  now uses `pragma_table_xinfo`, keeping `hidden` 0, 2 and 3. Check that your catalog query returns generated
  columns, and only the tables of the current database or schema.
- *Different type*: only for `strict_types` connectors. The catalog spells a type differently from the DDL, and
  `parse_type` reads them into different `DataType`s. Make `parse_type` read the catalog's canonical spelling
  (`Nullable(...)` wrappers, default precisions, aliases) into the same `DataType` as the DDL's.

## Timestamps and time zones

**Symptom**: `same_seed_same_data_for_any_batching` passes, but timestamps are off by hours compared with
`dbgen validate`'s samples. Or CHECKs comparing timestamps fail after the load.

`Value::Time` is UTC, and the text encodings write it without a zone. The server reads such text in the
session's time zone, so every connection must use UTC: PostgreSQL runs `SET TIME ZONE 'UTC'`, MySQL `SET
time_zone = '+00:00'`. Set it in the pool's `open`, not once, because the pool opens connections on demand.
Mind the precision as well: MySQL `DATETIME` without a precision holds whole seconds (`timestamp_precision:
0`). ClickHouse `DateTime` is 32-bit seconds and `DateTime64(p)` holds `p` digits. The `DataType` carries the
precision, so encode with it.

## Booleans

**Symptom**: a boolean column rejects `t`, or verification reports CHECK violations on booleans.

Spell booleans the way the server reads them in your bulk format: `t`/`f` in PostgreSQL's `COPY`, `1`/`0` in
MySQL and SQLite `INSERT`s (set in the connector's `TextStyle`). Reading back goes through
`CAST(x AS TEXT)`, and core reads `true`/`false`, `1`/`0` and `yes`/`no`.

## Escaping

**Symptom**: rows with quotes or backslashes fail to insert, or are stored with doubled backslashes.

The `TextStyle` must match the session. MySQL treats backslashes as escapes unless `sql_mode` contains
`NO_BACKSLASH_ESCAPES`, so the connector removes that mode when a connection opens and uses
`backslash_escapes: true`. Standard SQL only doubles quotes (`backslash_escapes: false`). PostgreSQL's `COPY`
text format has its own escapes for `\`, tabs and newlines, which `copy_field` applies. Quote identifiers with
`quote_ident`, never by hand: table and column names can be keywords (`order`) or contain spaces.

## Statement size limits

**Symptom**: large batches fail with "packet too big" or "statement too long", while small batch sizes work.

`insert_statements` cuts a batch into multi-row `INSERT`s of at most a given size: 4 MiB for MySQL, below
the default `max_allowed_packet` of 64 MiB, and 1 MiB for SQLite. Run all statements of a batch in one
transaction, so the batch stays atomic. A binary format sent in one request (ClickHouse) has no such limit
below the server's request size.

## Foreign key ordering

**Symptom**: inserts fail with foreign key violations, or `truncate` fails because other tables reference the
table.

- If the database checks foreign keys on insert, keep `enforces_foreign_keys` at `true`. Core then loads
  parents first, a self-referencing table one batch at a time in row order, and on resume reloads a table's
  dependents with it. Set it to `false` only if inserts do not check foreign keys at all (ClickHouse).
  Otherwise tables load in parallel in any order.
- SQLite leaves foreign key checks off by default, so dbgen turns them on in every connection: the data is
  checked like everywhere else.
- `truncate` must work despite foreign keys. PostgreSQL truncates all tables in one statement. MySQL sets
  `FOREIGN_KEY_CHECKS = 0` on the connection around the statements. SQLite sets `PRAGMA foreign_keys = OFF`
  around its `DELETE`s.

## Retries that never succeed, or loads that stop too early

**Symptom**: a load retries the same batch `max_retries` times with the same error. Or a load stops on a
deadlock that would have passed on a second try.

The error is misclassified. Classify by the server's error codes, as PostgreSQL does with SQLSTATE classes and
MySQL with error numbers, and keep deterministic errors (constraint violations, syntax errors, bad values)
`Failed`. If a request may have been stored before the failure was reported, such as a lost connection during
commit, `Transient` is still right. The retry may duplicate rows, but the final count then differs from the
target, and the next run reloads the table.

## Reference rows change between runs

**Symptom**: two runs with the same seed generate different foreign keys into a table marked `existing`.

`read_rows` must return the same rows in the same order: the default sorts on every selected column after
casting it to text. If you override it, sort the same way. A collapsing or deduplicating engine must read the
merged state (ClickHouse adds `FINAL`).

## The introspected schema differs from the declared one

**Symptom**: `introspection_round_trip` reports a column type, a key, or a sample with different repairs.

Catalogs print a normalized form, and some of it compiles to something other than the declaration. Write the
declared form back in `introspect_ddl`. The built-in connectors needed these rewrites:

- *Types*: MySQL prints `BOOLEAN` as `tinyint(1)`, and PostgreSQL prints `timestamptz` as `timestamp with time
  zone` and `serial` as an integer with a `nextval(...)` default. dbgen reads the printed forms as a small
  integer, a timestamp without zone, and a column with a database default.
- *Defaults*: MySQL prints `DEFAULT NULL` on nullable columns. dbgen reads that as a database default and would
  leave the column to the server.
- *CHECKs*: PostgreSQL adds casts (`(0)::numeric`, `'x'::text`) and prints `IN` lists as `= ANY (ARRAY[...])`.
  MySQL adds character set introducers (`_utf8mb4'x'`). The CHECK still compiles, but dbgen no longer sees its
  bounds or value list, so it repairs rows instead of generating them right, or cannot evaluate the CHECK at
  all. Rewrite the expression: PostgreSQL parses it with `sqlparser` and rewrites the tree, MySQL strips the
  introducers outside quoted text.

Prefer rewriting a parsed tree to rewriting text, and keep the original when the expression does not parse.

## Orderings between numeric columns

**Symptom**: a CHECK such as `max_salary > min_salary` fails often, or loads slowly.

This is core, not the connector. Orderings between columns are satisfied by repair, not by construction. Give
such columns ranges that do not overlap in the configuration (`columns.<table>.<column>: { range: [...] }`),
as the `hr` fixture does.
