//! SQL dialects: the handle that names a database family ([`Dialect`]), and the shared
//! DDL adapter ([`standard`]) a connector uses when `sqlparser` parses its statements whole.
//!
//! A dialect knows how to turn one statement of its SQL into dialect-neutral
//! [`Ddl`](crate::sql::Ddl) commands, which spellings mean which types, how to quote
//! identifiers, and which table definitions imply uniqueness. Those are methods of the
//! connector ([`Connector`](crate::connector::Connector)); this module holds what
//! connectors share.

pub mod standard;

pub use crate::connector::{Dialect, TableSemantics};

/// Compatibility path of the ClickHouse dialect adapter, from before connectors:
/// [`crate::connectors::clickhouse::dialect`].
#[cfg(feature = "clickhouse")]
pub use crate::connectors::clickhouse::dialect as clickhouse;
