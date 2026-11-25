//! dbgen: compiles SQL DDL into an in-memory schema, generates rows that satisfy it, and
//! bulk-loads them into PostgreSQL, MySQL, SQLite or ClickHouse.
//!
//! ```text
//! DDL files ──sqlparser + dialect adapter──> Ddl commands ──fold──> Schema
//! Schema + dbgen.yaml (+ rows of referenced tables) ──> Plan (World of generators)
//! Plan ──generator threads──> encoded batches ──writer threads──> database
//! ```
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`config`] | `dbgen.yaml` |
//! | [`connector`], [`connectors`] | The connector API, and one connector per database, listed in a registry |
//! | [`sql`], [`dialect`], [`schema`] | DDL text to an in-memory schema, through the connector's dialect |
//! | [`plan`], [`generator`], [`expr`], [`rng`] | How every column gets its values; CHECKs; determinism |
//! | [`db`] | The connected database ([`db::Backend`]) and the toolkit connectors share |
//! | [`engine`], [`state`] | The load pipeline, retries, resume |
//! | [`run`], [`verify`], [`report`], [`progress`] | The commands and their output |
//!
//! Everything outside [`connectors`] is the same for every database. `docs/CONNECTORS.md`
//! explains how to add one.

pub mod config;
pub mod connector;
pub mod connectors;
pub mod db;
pub mod dialect;
pub mod engine;
pub mod error;
pub mod expr;
pub mod generator;
pub mod plan;
pub mod progress;
pub mod report;
pub mod rng;
pub mod run;
pub mod schema;
pub mod sql;
pub mod state;
pub mod timeutil;
pub mod value;
pub mod verify;

/// The connector guide (`docs/`), whose Rust examples run as doctests so they keep up
/// with the API.
#[cfg(doctest)]
pub mod guide {
    #[doc = include_str!("../docs/CONNECTORS.md")]
    pub struct Connectors;
    #[doc = include_str!("../docs/connectors/tutorial.md")]
    pub struct Tutorial;
    #[doc = include_str!("../docs/connectors/reference.md")]
    pub struct Reference;
    #[doc = include_str!("../docs/connectors/types.md")]
    pub struct Types;
    #[doc = include_str!("../docs/connectors/testing.md")]
    pub struct Testing;
    #[doc = include_str!("../docs/connectors/maintaining.md")]
    pub struct Maintaining;
    #[doc = include_str!("../docs/connectors/troubleshooting.md")]
    pub struct Troubleshooting;
}
