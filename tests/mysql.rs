//! Conformance tests of the MySQL connector, against a real server.
//!
//! Set `DBGEN_TEST_MYSQL_URL` to the server without a database, e.g.
//! `mysql://root:dbgen@localhost:13306` (`docker compose up -d mysql` starts one). The
//! tests create, fill and drop databases whose names end in `_test`, and touch nothing
//! else. Without the variable only the offline tests run.

mod common;

use common::Server;
use dbgen::connectors::mysql::Mysql;
use dbgen::db::Backend;

fn server() -> Option<Server> {
    let base = match std::env::var("DBGEN_TEST_MYSQL_URL") {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => {
            eprintln!("DBGEN_TEST_MYSQL_URL not set: skipping");
            return None;
        }
    };
    let admin = format!("{base}/mysql");
    Some(Server {
        connector: "mysql",
        url: Box::new(move |db| format!("{base}/{db}")),
        drop: Box::new(move |db| Mysql::connect(&admin).unwrap().execute(&format!("DROP DATABASE IF EXISTS {db}")).unwrap()),
        unknown_database: true,
        atomic_batches: true,
        introspection: true,
    })
}

// Deleting orders cascades to their lines: both tables are reloaded.
conformance_tests! {
    connector: "mysql",
    server: server,
    prefix: "my",
    resume_delete: "DELETE FROM orders WHERE order_id % 7 = 0",
}
