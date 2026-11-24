//! dbgen: compiles SQL DDL into an in-memory schema, generates rows that satisfy it, and
//! bulk-loads them into the database.
//!
//! ```text
//! DDL files ──sqlparser + dialect adapter──> Ddl commands ──fold──> Schema
//! Schema + dbgen.yaml (+ rows of referenced tables) ──> Plan (World of generators)
//! Plan ──generator threads──> encoded batches ──writer threads──> database
//! ```

pub mod config;
pub mod db;
pub mod dialect;
pub mod engine;
pub mod error;
pub mod expr;
pub mod gen;
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
