//! Conformance tests of the SQLite connector, on files in a temporary directory. SQLite
//! needs no server, so these always run.

mod common;

use std::path::PathBuf;

use common::Server;

fn server() -> Option<Server> {
    let dir: PathBuf = std::env::temp_dir().join(format!("dbgen-it-sqlite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files = dir.clone();
    Some(Server {
        connector: "sqlite",
        url: Box::new(move |db| format!("sqlite:{}", dir.join(format!("{db}.db")).display())),
        drop: Box::new(move |db| {
            for suffix in ["db", "db-wal", "db-shm", "db-journal"] {
                let _ = std::fs::remove_file(files.join(format!("{db}.{suffix}")));
            }
        }),
        // Opening a missing file creates it.
        unknown_database: false,
        atomic_batches: true,
        introspection: true,
    })
}

// Lines are a leaf of the references: only they are reloaded.
conformance_tests! {
    connector: "sqlite",
    server: server,
    prefix: "lite",
    resume_delete: "DELETE FROM order_lines WHERE quantity > 10",
}
