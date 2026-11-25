# Type mapping

A column's type makes two trips through a connector:

```text
DDL:     "numeric(10, 2)" --sqlparser--> sqlparser DataType --convert::data_type + TypeRules--> dbgen DataType
load:    dbgen DataType + generated Value --Backend::encode_row--> bytes in the bulk format --> stored value
catalog: live type string --Backend::parse_type (= Connector::parse_type)--> dbgen DataType   (strict_types)
```

Core generates values from the dbgen [`DataType`](../../src/schema/types.rs) alone, so it is the contract
between the two trips. Whatever the DDL spelled, the connector must store the value the `DataType` promises.

## From DDL to `DataType`

The shared adapter hands each type to `sqlparser`, then to
[`convert::data_type`](../../src/sql/convert.rs), which reads it according to the connector's `TypeRules`.
The same spelling can mean different things in different dialects:

| Field | `STANDARD` | Meaning | Set by |
|---|---|---|---|
| `int8_is_8_bits` | `false` | `Int8` is 8 bits (ClickHouse), not PostgreSQL's 64-bit `int8`. | ClickHouse |
| `nullable_by_default` | `true` | A column without `NOT NULL` accepts NULL (standard SQL), rather than only when its type says `Nullable(...)`. | ClickHouse (`false`) |
| `varchar_has_length` | `true` | `VARCHAR(n)` limits the length, rather than being an alias of an unbounded string. | ClickHouse (`false`) |
| `date_is_16_bit` | `false` | `DATE` is a 16-bit day count (1970-2149), rather than a full-range date. | ClickHouse |
| `timestamp_is_seconds` | `false` | `TIMESTAMP`/`DATETIME` without precision is 32-bit seconds. | ClickHouse |
| `timestamp_precision` | 6 | Fractional digits of `TIMESTAMP`/`DATETIME` without a precision. | MySQL, SQLite (0) |
| `custom_type` | none | Type names `sqlparser` does not know. | PostgreSQL, ClickHouse, MySQL, SQLite |
| `identity_types` | none | Custom names that also make the column `NOT NULL` with an identity default. | PostgreSQL (`serial`...) |

### Names `sqlparser` does not know

`sqlparser` parses an unknown type name as a custom type, with its modifiers. `custom_type` maps the names your
database has. A function that returns `None` leaves the column `Unsupported` (see below). This block is a
doctest:

```rust
use dbgen::dialect::standard;
use dbgen::schema::DataType;
use dbgen::sql::convert::TypeRules;
use sqlparser::ast::TypeModifier;
use sqlparser::dialect::GenericDialect;

/// Type names of an imaginary database.
fn custom_type(name: &str, mods: &[TypeModifier], _rules: &TypeRules) -> Option<DataType> {
    let number = |i: usize| match mods.get(i) {
        Some(TypeModifier::Number(n, _)) => n.parse::<u32>().ok(),
        _ => None,
    };
    Some(match name.to_ascii_lowercase().as_str() {
        "text2" => DataType::String { max_len: number(0) },
        "autoid" => DataType::Int { bits: 64, signed: true },
        _ => return None,
    })
}

const RULES: TypeRules = TypeRules { custom_type, identity_types: &["autoid"], ..TypeRules::STANDARD };

let sql = GenericDialect {};
assert_eq!(standard::parse_type("TEXT2(40)", &sql, &RULES).unwrap(), DataType::String { max_len: Some(40) });
assert!(matches!(standard::parse_type("GEOGRAPHY", &sql, &RULES).unwrap(), DataType::Unsupported(_)));
```

Keep the list to your database's own names. MySQL and SQLite use `convert::compat_custom_type`, the union of
PostgreSQL and ClickHouse names that every dialect accepted before connectors existed. It is kept so their DDL
compiles exactly as before, and it is not a model for new connectors.

### Nullability, defaults, identities

- `Nullable(T)` wraps the type of a column that accepts NULL. With `nullable_by_default`, any column without
  `NOT NULL` gets it.
- `DEFAULT expr` is `DefaultKind::Default`. An identity (`AUTO_INCREMENT`, `GENERATED ... AS IDENTITY`,
  `serial`) is `DefaultKind::Identity`. Core leaves both to the database, unless `generation.fill_defaults` is
  set, or the column is configured or takes part in a key, CHECK or reference. An identity primary key is
  therefore generated like any key, and `finish_load` should move its sequence past the loaded values.
- A generated column (`GENERATED ALWAYS AS`, `MATERIALIZED`, `ALIAS`) is never inserted. It must still appear
  in `live_columns`.

## From `DataType` to the wire

The value core hands to `encode_row` for each `DataType`, and how the toolkit's text encodings write it:

| `DataType` | `Value` | Text (`scalar_text`) |
|---|---|---|
| `Bool` | `Bool` | the `TextStyle`'s `true_text` / `false_text` (`COPY`: `t`/`f`) |
| `Int { bits, signed }` | `Int(i128)` | decimal |
| `Float { bits }` | `Float(f64)` | shortest round-trip form; non-finite values are refused |
| `Decimal { precision, scale }` | `Decimal(unscaled, scale)` | exact, with `scale` digits |
| `String { max_len }`, `FixedString(n)` | `Str` | as is; length within `max_len` characters |
| `Uuid` | `Uuid(u128)` | `8-4-4-4-12` hex |
| `Date`, `Date32` | `Date(days since 1970-01-01)` | `YYYY-MM-DD` |
| `DateTime { precision, tz }` | `Time(µs since the epoch, UTC)` | `YYYY-MM-DD HH:MM:SS[.f]`, `precision` digits (0 when `None`), UTC |
| `Enum { variants }` | `Str` (a variant's name) | the name |
| `Ipv4`, `Ipv6` | `Ip4(u32)`, `Ip6(u128)` | dotted / colon form |
| `Json` | `Str` (JSON text) | as is |
| `Array(T)` | `Array` | `COPY`: `{...}`; SQL literals: refused |
| `Map(K, V)`, `Tuple(...)` | `Map`, `Tuple` | refused by the text encodings |
| `Nullable(T)`, `LowCardinality(T)` | `Null` or a value of `T` | `NULL` / `\N` |

Rules for `encode_row`:

- **Encode exactly or refuse.** A value that does not fit the column, such as an integer out of range, an
  array where the format has none, or a non-finite float, is a `CellError` with the column's position. Never
  round, truncate or clamp silently: the data would no longer be what the seed promises.
- **Times are UTC.** `Value::Time` is UTC microseconds. Write it in UTC, and set the session to UTC so the
  server does not shift it (see [troubleshooting](troubleshooting.md#timestamps-and-time-zones)).
- **Decimals are exact.** Use the unscaled integer and the scale, never `f64`.
- A binary format encodes from the column's `DataType`, as ClickHouse's `encode` does, and sets `strict_types`.
  The `DataType` then decides the byte layout, so the live column must have exactly that type.

## Back from the catalog

`live_columns` returns types as the catalog spells them. They are compared with the DDL only when
`strict_types` is set: core parses each one with `Backend::parse_type` and requires equality with the
compiled `DataType`. For a strict connector, `parse_type` must read the catalog's canonical spelling into the
same `DataType` as the DDL's spelling. For example, ClickHouse reports `Nullable(DateTime64(6, 'UTC'))` for a
column declared `DateTime64(6, 'UTC') NULL`. `read_rows` brings values back as text, and core casts the text
to the column's `DataType`. Booleans must therefore read back as `true`/`false`, `1`/`0` or `yes`/`no`.

## Types a connector cannot generate

Map a type you cannot represent to `DataType::Unsupported(spelling)`, rather than to a near miss that would
encode wrong values. Core then:

- leaves a nullable column NULL, with a warning;
- leaves a column with a default to the database;
- reports a validation problem for a `NOT NULL` column without a default, naming it.

A user can still set such a column with `columns.<table>.<column>: { value: ... }` or skip it with
`generate: false`.
