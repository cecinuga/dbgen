//! The connector API: what dbgen needs from a database, and the handle the rest of the
//! program uses to reach it.
//!
//! A connector is the part of dbgen that works with one database. It has two halves:
//!
//! | Half | Lives | Knows |
//! |---|---|---|
//! | [`Connector`] | for the whole program, no connection | names and URL schemes, the SQL dialect of the DDL, capabilities; opens a [`Backend`] |
//! | [`Backend`] | one per target database, holds connections | the catalog, the bulk format, statements, counts |
//!
//! The halves are separate because DDL compiles without a database (`dbgen inspect`,
//! planning), while a [`Backend`] exists only once a URL is known.
//!
//! Everything else (configuration, planning, generation, the load engine, resume,
//! verification, reporting) is the same for every database and reaches the connector only
//! through a [`Dialect`]: a copyable handle on a registered [`Connector`]. Connectors are
//! listed once, in [`crate::connectors`]. The guide for writing one is
//! `docs/CONNECTORS.md`.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sqlparser::dialect::Dialect as SqlDialect;

use crate::config::Config;
use crate::db::Backend;
use crate::dialect::standard;
use crate::error::Result;
use crate::schema::{DataType, Key};
use crate::sql::convert::TypeRules;
use crate::sql::{self, Ddl, Diag, EngineSpec, ExprSpec, Source, Statement, TableDef};

/// A database dbgen can work with: its names, the SQL its DDL is written in, what it
/// implies, and how to connect to it.
///
/// Only [`name`](Connector::name), [`product`](Connector::product),
/// [`schemes`](Connector::schemes), [`connect`](Connector::connect),
/// [`sql`](Connector::sql) and [`type_rules`](Connector::type_rules) are required. The
/// defaults parse DDL with `sqlparser` through the shared adapter
/// ([`crate::dialect::standard`]), quote identifiers the ANSI way, and describe a database
/// whose tables are plain row stores with enforced foreign keys: what PostgreSQL, MySQL
/// and SQLite need.
///
/// Implementations are unit structs registered as a [`Dialect`], so every method takes
/// `&self` and may be called from any thread.
pub trait Connector: Sync + 'static {
    /// The name in configurations (`dialect: postgres`), on the command line
    /// (`--dialect postgres`), in the compiled schema's JSON and in fixture file names
    /// (`tests/fixtures/<domain>/postgres.sql`). Lowercase, unique among connectors.
    fn name(&self) -> &'static str;

    /// The product name, for messages: `PostgreSQL`.
    fn product(&self) -> &'static str;

    /// Other names accepted wherever [`name`](Connector::name) is (`pg`, `postgresql`).
    /// Lowercase, unique among connectors.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// URL schemes that select this connector when the configuration has no `dialect`
    /// (`postgres` for `postgres://...`). Lowercase, unique among connectors.
    fn schemes(&self) -> &'static [&'static str];

    /// Opens the database at `url`. Connections should be opened lazily, so this fails
    /// only on a malformed URL ([`Error::Config`](crate::error::Error::Config));
    /// [`Backend::version`] is the first call expected to reach the server. `config` is the
    /// whole configuration, for the settings a connector needs (`base_dir` to resolve a
    /// relative file path, `generation.http_timeout_secs`).
    fn connect(&self, url: &str, config: &Config) -> Result<Box<dyn Backend>>;

    /// The `sqlparser` dialect that tokenizes and parses this SQL.
    fn sql(&self) -> &'static dyn SqlDialect;

    /// How this dialect reads type spellings and column definitions.
    fn type_rules(&self) -> TypeRules;

    /// Splits a DDL file into statements. The default splits at top-level `;`, tokenizing
    /// with [`sql`](Connector::sql). Override it for scripts with client-side separators
    /// (`DELIMITER`, `GO`).
    fn split<'s>(&self, source: &'s Source) -> std::result::Result<Vec<Statement<'s>>, Diag> {
        sql::split(source, self.sql())
    }

    /// Reduces one statement to its effect on the schema. The default hands the statement
    /// to `sqlparser` whole ([`standard::parse_statement`]); a dialect whose DDL
    /// `sqlparser` cannot parse whole drives it on parts of the statement instead, as
    /// ClickHouse does.
    fn parse_statement(&self, stmt: &Statement) -> std::result::Result<Ddl, Diag> {
        standard::parse_statement(stmt, self.sql(), &self.type_rules())
    }

    /// Parses a type as the database spells it, in DDL or in its catalog
    /// ([`Backend::live_columns`]).
    fn parse_type(&self, s: &str) -> std::result::Result<DataType, String> {
        standard::parse_type(s, self.sql(), &self.type_rules())
    }

    /// Quotes an identifier. The default is ANSI: `"name"`, with `"` doubled.
    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// Keys and fixed columns implied by a table's engine (`ENGINE = ...`), beyond its
    /// declared constraints. The default implies nothing.
    fn table_semantics(&self, _engine: Option<&EngineSpec>, _order_by: &[ExprSpec]) -> TableSemantics {
        TableSemantics::default()
    }

    /// Whether `def` creates a table that stores generated rows (rather than a view, a
    /// proxy or an integration). The default: every table does.
    fn is_storage_table(&self, _def: &TableDef) -> bool {
        true
    }

    /// Whether the database checks foreign keys on insert. Then parents are loaded before
    /// their children, a self-referencing table one batch at a time, and a table reloaded
    /// on resume takes its dependents with it. The default: it does.
    fn enforces_foreign_keys(&self) -> bool {
        true
    }
}

/// What a table definition implies beyond its columns.
#[derive(Debug, Default)]
pub struct TableSemantics {
    /// Keys whose values must be unique for rows to survive (beyond PRIMARY KEY/UNIQUE).
    pub keys: Vec<Key>,
    /// Columns the engine requires to hold a fixed value (e.g. ClickHouse's `is_deleted`
    /// of `ReplacingMergeTree` must be 0, or the row reads as deleted).
    pub fixed: Vec<(String, i64)>,
    /// Rows with equal sorting keys are merged, so reads must deduplicate (`FINAL`).
    pub collapsing: bool,
}

/// A registered connector, by handle: the database family of a configuration, the dialect
/// of its DDL and the kind of database loaded.
///
/// It dereferences to the [`Connector`], so `dialect.parse_type(..)` and
/// `dialect.name()` call the connector. Handles compare equal when their names are, and
/// serialize as the name.
#[derive(Clone, Copy)]
pub struct Dialect(&'static dyn Connector);

impl Dialect {
    /// The handle of `connector`. Each connector module defines one as `CONNECTOR`.
    pub const fn new(connector: &'static dyn Connector) -> Dialect {
        Dialect(connector)
    }

    /// The connector itself.
    pub fn connector(self) -> &'static dyn Connector {
        self.0
    }

    /// Every connector in this build, in registry order.
    pub fn all() -> &'static [Dialect] {
        crate::connectors::REGISTRY
    }

    /// The connector called `name` or one of its aliases, ignoring case.
    pub fn from_name(name: &str) -> Option<Dialect> {
        Dialect::all().iter().copied().find(|d| d.name().eq_ignore_ascii_case(name) || d.aliases().iter().any(|a| a.eq_ignore_ascii_case(name)))
    }

    /// The connector of a database URL scheme (`postgres` of `postgres://...`), ignoring
    /// case. The first connector in registry order that claims it.
    pub fn from_scheme(scheme: &str) -> Option<Dialect> {
        Dialect::all().iter().copied().find(|d| d.schemes().iter().any(|s| s.eq_ignore_ascii_case(scheme)))
    }

    /// Every name and alias, for messages: `clickhouse (ch), postgres (postgresql, pg)`.
    pub fn known_names() -> String {
        let names: Vec<String> = Dialect::all()
            .iter()
            .map(|d| match d.aliases() {
                [] => d.name().to_string(),
                aliases => format!("{} ({})", d.name(), aliases.join(", ")),
            })
            .collect();
        if names.is_empty() {
            "none: this build has no connectors".into()
        } else {
            names.join(", ")
        }
    }

    /// Every URL scheme, for messages: `postgres:, postgresql: (PostgreSQL); ...`.
    pub fn known_schemes() -> String {
        let schemes: Vec<String> = Dialect::all()
            .iter()
            .map(|d| format!("{} ({})", d.schemes().iter().map(|s| format!("{s}:")).collect::<Vec<_>>().join(", "), d.product()))
            .collect();
        if schemes.is_empty() {
            "none: this build has no connectors".into()
        } else {
            schemes.join("; ")
        }
    }
}

impl Deref for Dialect {
    type Target = dyn Connector;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl PartialEq for Dialect {
    fn eq(&self, other: &Dialect) -> bool {
        self.name() == other.name()
    }
}

impl Eq for Dialect {}

impl fmt::Debug for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for Dialect {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Dialect {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Dialect, D::Error> {
        let name = String::deserialize(deserializer)?;
        Dialect::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown dialect '{name}', expected one of: {}", Dialect::known_names())))
    }
}

impl clap::ValueEnum for Dialect {
    fn value_variants<'a>() -> &'a [Dialect] {
        Dialect::all()
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.name()).aliases(self.aliases().iter().copied()))
    }
}
