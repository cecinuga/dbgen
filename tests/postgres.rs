//! Conformance tests of the PostgreSQL connector, against a real server.
//!
//! Set `DBGEN_TEST_POSTGRES_URL` to the server without a database, e.g.
//! `postgres://dbgen:dbgen@localhost:15432` (`docker compose up -d postgres` starts one).
//! The tests create, fill and drop databases whose names end in `_test`, and touch
//! nothing else. Without the variable only the offline tests run.

mod common;

use common::Server;
use dbgen::connectors::postgres::Postgres;
use dbgen::db::Backend;

fn server() -> Option<Server> {
    let base = match std::env::var("DBGEN_TEST_POSTGRES_URL") {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => {
            eprintln!("DBGEN_TEST_POSTGRES_URL not set: skipping");
            return None;
        }
    };
    let admin = format!("{base}/postgres");
    Some(Server {
        connector: "postgres",
        url: Box::new(move |db| format!("{base}/{db}")),
        drop: Box::new(move |db| {
            let pg = Postgres::connect(&admin).unwrap();
            pg.execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).unwrap();
        }),
        unknown_database: true,
        atomic_batches: true,
        introspection: true,
    })
}

// Deleting orders cascades to their lines: both tables are reloaded.
conformance_tests! {
    connector: "postgres",
    server: server,
    prefix: "pg",
    resume_delete: "DELETE FROM orders WHERE order_id % 7 = 0",
}
