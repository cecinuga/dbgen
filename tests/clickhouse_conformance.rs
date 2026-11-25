//! Conformance tests of the ClickHouse connector, against a real server: the scenarios of
//! the suite that do not need the shop fixture of the SQL databases. The shop scenarios
//! (end to end, determinism, resume) are in `tests/clickhouse.rs`, on ClickHouse's own
//! shop fixture.
//!
//! Set `DBGEN_TEST_CLICKHOUSE_URL` to the server without a database, e.g.
//! `http://dbgen:dbgen@localhost:18123`. The tests create, fill and drop databases whose
//! names end in `_test`, and touch nothing else. Without the variable only the offline
//! tests run.

mod common;

use common::Server;
use dbgen::config::{Config, Overrides};
use dbgen::connectors::clickhouse::Client;
use dbgen::db::Database;
use dbgen::run::{self, GenerateOptions};

fn server() -> Option<Server> {
    let base = match std::env::var("DBGEN_TEST_CLICKHOUSE_URL") {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => {
            eprintln!("DBGEN_TEST_CLICKHOUSE_URL not set: skipping");
            return None;
        }
    };
    let admin = Client::connect(&format!("{base}/default"), 60).unwrap();
    Some(Server {
        connector: "clickhouse",
        url: Box::new(move |db| format!("{base}/{db}")),
        drop: Box::new(move |db| admin.execute(&format!("DROP DATABASE IF EXISTS {db} SYNC")).unwrap()),
        unknown_database: true,
        // The primary key is a sorting key: duplicates are accepted.
        atomic_batches: false,
        introspection: true,
    })
}

#[test]
fn fixtures_compile() {
    common::fixtures_compile("clickhouse");
}

#[test]
fn missing_database_is_reported() {
    let Some(s) = server() else { return };
    common::missing_database(&s, "dbgen_it_ch_missing_test");
}

#[test]
fn every_domain_end_to_end() {
    let Some(s) = server() else { return };
    common::domains_end_to_end(&s, "ch");
}

#[test]
fn introspection_round_trip() {
    let Some(s) = server() else { return };
    common::introspection_round_trip(&s, "ch");
}

/// ClickHouse stores CHECKs in its own normal form: `CASE x WHEN ...` comes back as
/// `caseWithExpression(x, ...)`. The shop schema read back from the server must plan with
/// every CHECK evaluated, and load without DDL files.
#[test]
fn shop_read_back_from_the_server_loads() {
    let Some(s) = server() else { return };
    let name = "dbgen_it_ch_shop_read_test";
    (s.drop)(name);
    let shop = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop.sql");
    let with_ddl = Overrides { database: Some((s.url)(name)), schema: vec![shop], ..Overrides::default() };
    run::apply_config(&Config::resolve(None, &with_ddl).unwrap(), true).unwrap();

    let mut without_ddl = Overrides {
        database: Some((s.url)(name)),
        // The INSERT that seeds countries is not part of the schema the server reports.
        rows: ["countries=existing", "customers=500", "products=100", "orders=2000", "order_lines=4000"].map(String::from).to_vec(),
        ..Overrides::default()
    };
    without_ddl.set("end_time", Some("2026-01-01"));
    without_ddl.set_path("state_file", Some(&std::env::temp_dir().join(format!("dbgen-it-{}-{name}.json", std::process::id())))).unwrap();
    let config = Config::resolve(None, &without_ddl).unwrap();
    let schema = run::load_schema(&config).unwrap();
    let db = Database::connect(&config).unwrap();
    let plan = dbgen::plan::build(&config, &schema, Some(&db)).unwrap();
    assert!(!plan.warnings.iter().any(|w| w.contains("not evaluated")), "{:#?}", plan.warnings);

    let opts = GenerateOptions { config_path: config.path.clone(), reset: false, dry_run: false, report: None, progress_bars: false, verify: true };
    let outcome = run::generate_config(config, &opts).unwrap();
    assert!(outcome.error.is_none(), "{:?}\n{}", outcome.error, outcome.verification.unwrap_or_default());
    (s.drop)(name);
}
