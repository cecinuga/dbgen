//! Failure classes. Each maps to a distinct process exit code (see `main.rs`), so a script
//! driving a series of loads can tell a bad config from a failed load.

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    /// The DDL input could not be compiled into a schema.
    #[error("schema error: {0}")]
    Schema(String),
    /// The configuration and the schema cannot produce the requested data, or the live
    /// database does not match the schema.
    #[error("validation failed:\n{0}")]
    Validation(String),
    #[error("cannot connect to the database: {0}")]
    Connect(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("generation failed: {0}")]
    Generation(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("interrupted")]
    Interrupted,
}

/// The result of a command step.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The process exit code: 2 for configuration, 3 for connection, 1 for load or
    /// verification, 130 for an interrupt.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Config(_) | Error::Schema(_) | Error::Validation(_) => 2,
            Error::Connect(_) => 3,
            Error::Db(_) | Error::Generation(_) | Error::Verification(_) => 1,
            Error::Interrupted => 130,
        }
    }
}
