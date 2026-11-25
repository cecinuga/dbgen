//! PostgreSQL, over its native protocol, without TLS.
//!
//! - DDL: the shared adapter ([`crate::dialect::standard`]) with `sqlparser`'s PostgreSQL
//!   dialect, plus the PostgreSQL type names `sqlparser` leaves as custom (`serial`,
//!   `inet`, `citext`, `timestamptz`...).
//! - Loading: [`backend`], `COPY ... FROM STDIN` in text format, one `COPY` per batch.
//!   After a load, identity sequences move past the loaded keys and statistics are
//!   refreshed.
//! - Introspection: [`introspect`], the DDL of the current schema rebuilt from the
//!   catalogs.

pub mod backend;
pub mod introspect;

use sqlparser::ast::TypeModifier;
use sqlparser::dialect::{Dialect as SqlDialect, PostgreSqlDialect};

pub use backend::Postgres;

use crate::config::Config;
use crate::connector::{Connector, Dialect};
use crate::db::Backend;
use crate::error::Result;
use crate::schema::DataType;
use crate::sql::convert::TypeRules;

/// The PostgreSQL connector.
pub struct PostgresConnector;

/// The handle of the PostgreSQL connector.
pub const CONNECTOR: Dialect = Dialect::new(&PostgresConnector);

#[allow(non_upper_case_globals)]
impl Dialect {
    /// PostgreSQL, as it was named when dialects were an enum.
    pub const Postgres: Dialect = CONNECTOR;
}

static SQL: PostgreSqlDialect = PostgreSqlDialect {};

/// Standard SQL rules with the PostgreSQL type names.
pub const RULES: TypeRules = TypeRules { custom_type, identity_types: SERIAL_TYPES, ..TypeRules::STANDARD };

/// Serial types: integers whose default is a sequence, so the column is an identity.
const SERIAL_TYPES: &[&str] = &["smallserial", "serial2", "serial", "serial4", "bigserial", "serial8"];

/// PostgreSQL types that `sqlparser` reads as custom names.
fn custom_type(name: &str, _mods: &[TypeModifier], rules: &TypeRules) -> Option<DataType> {
    let int = |bits: u16| DataType::Int { bits, signed: true };
    Some(match name.to_ascii_lowercase().as_str() {
        "smallserial" | "serial2" => int(16),
        "serial" | "serial4" => int(32),
        "bigserial" | "serial8" => int(64),
        "inet" | "cidr" => DataType::Ipv4,
        "citext" | "name" => DataType::String { max_len: None },
        "money" => DataType::Decimal { precision: 19, scale: 2 },
        "timestamptz" => DataType::DateTime { precision: Some(rules.timestamp_precision), tz: Some("UTC".into()) },
        _ => return None,
    })
}

impl Connector for PostgresConnector {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn product(&self) -> &'static str {
        "PostgreSQL"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["postgresql", "pg"]
    }

    fn schemes(&self) -> &'static [&'static str] {
        &["postgres", "postgresql", "pg"]
    }

    fn connect(&self, url: &str, _config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Postgres::connect(url)?))
    }

    fn sql(&self) -> &'static dyn SqlDialect {
        &SQL
    }

    fn type_rules(&self) -> TypeRules {
        RULES
    }
}
