//! MySQL and MariaDB, over their native protocol, without TLS.
//!
//! - DDL: the shared adapter ([`crate::dialect::standard`]) with `sqlparser`'s MySQL
//!   dialect. `DATETIME` and `TIMESTAMP` without a precision hold whole seconds.
//! - Loading: [`backend`], multi-row `INSERT` statements, all statements of a batch in
//!   one transaction; statements stay below the server's packet limit.
//! - Introspection: [`introspect`], `SHOW CREATE TABLE` of every table, parents first.

pub mod backend;
pub mod introspect;

use sqlparser::dialect::{Dialect as SqlDialect, MySqlDialect};

pub use backend::Mysql;

use crate::config::Config;
use crate::connector::{Connector, Dialect};
use crate::db::Backend;
use crate::error::Result;
use crate::sql::convert::{self, TypeRules};

/// The MySQL connector.
pub struct MysqlConnector;

/// The handle of the MySQL connector.
pub const CONNECTOR: Dialect = Dialect::new(&MysqlConnector);

#[allow(non_upper_case_globals)]
impl Dialect {
    /// MySQL, as it was named when dialects were an enum.
    pub const Mysql: Dialect = CONNECTOR;
}

static SQL: MySqlDialect = MySqlDialect {};

/// Standard SQL rules; timestamps without a precision hold whole seconds. The custom type
/// names are the legacy set every dialect accepted before connectors (see
/// [`convert::compat_custom_type`]).
pub const RULES: TypeRules = TypeRules {
    timestamp_precision: 0,
    custom_type: convert::compat_custom_type,
    identity_types: convert::COMPAT_IDENTITY_TYPES,
    ..TypeRules::STANDARD
};

impl Connector for MysqlConnector {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn product(&self) -> &'static str {
        "MySQL"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["mariadb"]
    }

    fn schemes(&self) -> &'static [&'static str] {
        &["mysql", "mariadb"]
    }

    fn connect(&self, url: &str, _config: &Config) -> Result<Box<dyn Backend>> {
        Ok(Box::new(Mysql::connect(url)?))
    }

    fn sql(&self) -> &'static dyn SqlDialect {
        &SQL
    }

    fn type_rules(&self) -> TypeRules {
        RULES
    }

    fn quote_ident(&self, name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }
}
