//! The connector conformance suite, shared by the integration tests of every connector.
//!
//! A connector's test file defines how to reach a server ([`Server`]) and invokes
//! [`conformance_tests!`](crate::conformance_tests), which emits one test per scenario:
//!
//! | Test | Needs a server | Proves |
//! |---|---|---|
//! | `fixtures_compile` | no | the shop fixture and every domain fixture compile and plan in the connector's dialect |
//! | `shop_end_to_end` | yes | a load passes every verification check and the database holds the target |
//! | `same_seed_same_data_for_any_batching` | yes | the rows depend on the seed only, not on batch size or workers |
//! | `resume_reloads_tables_that_lost_rows` | yes | exact counts drive resume: lost rows come back, identical |
//! | `batches_are_atomic` | yes | a batch that fails leaves no row behind, and its error is not transient |
//! | `missing_database_is_reported` | yes | a missing database is `UnknownDatabase`, which `--create-database` fixes |
//! | `every_domain_end_to_end` | yes | every `tests/fixtures/<domain>/<connector>.sql` applies, loads and verifies |
//! | `introspection_round_trip` | yes | every fixture, applied, reads back through `introspect_ddl` as the same schema, which plans the same rows |
//!
//! The shop scenarios run `tests/fixtures/shop.<connector>.sql`; the domain matrix runs
//! every `tests/fixtures/<domain>/` that has a `config.yaml` and a `<connector>.sql`, with
//! the `database` and `schema` keys prepended to the configuration.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use dbgen::config::Config;
use dbgen::db::{Batch, Database, DbErrorKind, RowRef};
use dbgen::plan::{self, RowSource};
use dbgen::run::{self, GenerateOptions, Outcome};
use dbgen::schema::{self, DataType, KeyPart, Schema};
use dbgen::sql::{DefaultKind, Source};

/// Generated tables of the shop fixtures, parents first.
pub const TABLES: [&str; 5] = ["categories", "customers", "products", "orders", "order_lines"];

/// Rows the shop configuration generates.
pub const TARGET_ROWS: u64 = 40 + 2000 + 300 + 20_000 + 50_000;

/// The shop configuration, without `database` and `schema`.
const SHOP: &str = r#"
generation: { seed: 7, batch_size: BATCH, workers: WORKERS, history_days: 90, end_time: "2026-01-01" }
tables:
  categories: 40
  customers: 2000
  products: 300
  orders: 20000
  order_lines: 50000
  countries: existing
columns:
  orders.channel: { values: [store, web, app], weights: [2, 5, 3] }
"#;

/// A small shop, for scenarios that need rows but not many.
const SMALL_SHOP: &str = r#"
generation: { seed: 7, batch_size: 100, workers: 2, history_days: 90, end_time: "2026-01-01" }
tables:
  categories: 10
  customers: 50
  products: 20
  orders: 100
  order_lines: 200
  countries: existing
"#;

/// A database server to test against.
pub struct Server {
    /// The connector under test, by name: its fixtures are `tests/fixtures/shop.<connector>.sql`
    /// and `tests/fixtures/<domain>/<connector>.sql`.
    pub connector: &'static str,
    /// The URL of database `name`.
    pub url: Box<dyn Fn(&str) -> String>,
    /// Drops database `name` if it exists.
    pub drop: Box<dyn Fn(&str)>,
    /// Whether connecting to a database that does not exist can fail (servers with a
    /// catalog of databases; not SQLite, which creates the file).
    pub unknown_database: bool,
    /// Whether a duplicate primary key fails an insert, so a failed batch can be provoked
    /// (not ClickHouse, whose primary key is a sorting key).
    pub atomic_batches: bool,
    /// Whether the connector reads the schema back from the database
    /// (`Backend::introspect_ddl`), which is optional.
    pub introspection: bool,
}

/// Emits the conformance tests of a connector. `server` is a function returning
/// `Option<Server>` (`None` when no server is configured: the tests that need one pass
/// without doing anything); `prefix` keeps database names apart between connectors;
/// `resume_delete` is a statement that deletes some order lines.
#[macro_export]
macro_rules! conformance_tests {
    (connector: $connector:literal, server: $server:path, prefix: $prefix:literal, resume_delete: $delete:expr $(,)?) => {
        #[test]
        fn fixtures_compile() {
            $crate::common::fixtures_compile($connector);
        }

        #[test]
        fn shop_end_to_end() {
            let Some(s) = $server() else { return };
            $crate::common::end_to_end(&s, concat!("dbgen_it_", $prefix, "_shop_test"));
        }

        #[test]
        fn same_seed_same_data_for_any_batching() {
            let Some(s) = $server() else { return };
            $crate::common::deterministic(&s, concat!("dbgen_it_", $prefix, "_det_a_test"), concat!("dbgen_it_", $prefix, "_det_b_test"));
        }

        #[test]
        fn resume_reloads_tables_that_lost_rows() {
            let Some(s) = $server() else { return };
            $crate::common::resume(&s, concat!("dbgen_it_", $prefix, "_resume_test"), $delete);
        }

        #[test]
        fn batches_are_atomic() {
            let Some(s) = $server() else { return };
            $crate::common::atomic_batches(&s, concat!("dbgen_it_", $prefix, "_atomic_test"));
        }

        #[test]
        fn missing_database_is_reported() {
            let Some(s) = $server() else { return };
            $crate::common::missing_database(&s, concat!("dbgen_it_", $prefix, "_missing_test"));
        }

        #[test]
        fn every_domain_end_to_end() {
            let Some(s) = $server() else { return };
            $crate::common::domains_end_to_end(&s, $prefix);
        }

        #[test]
        fn introspection_round_trip() {
            let Some(s) = $server() else { return };
            $crate::common::introspection_round_trip(&s, $prefix);
        }
    };
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A fresh directory for one test's configuration, so no resume state is shared.
fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbgen-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `dbgen.yaml` for database `name` of `server` with DDL `schema` and the rest of
/// the configuration `body`, and creates the database with its schema.
fn apply_config(server: &Server, name: &str, schema: &Path, body: &str) -> dbgen::error::Result<PathBuf> {
    assert!(name.ends_with("_test"), "tests only touch *_test databases");
    (server.drop)(name);
    let path = test_dir(name).join("dbgen.yaml");
    let yaml = format!("database: \"{}\"\nschema: \"{}\"\n{body}", (server.url)(name), schema.display());
    std::fs::write(&path, yaml).unwrap();
    run::apply(&path, true)?;
    Ok(path)
}

fn shop_fixture(connector: &str) -> PathBuf {
    fixtures_dir().join(format!("shop.{connector}.sql"))
}

/// Writes a configuration for the shop fixture in a fresh directory (so no resume state
/// is shared between tests) and creates the database with its schema.
pub fn shop_config(server: &Server, name: &str, batch_size: u64, workers: usize) -> PathBuf {
    let body = SHOP.replace("BATCH", &batch_size.to_string()).replace("WORKERS", &workers.to_string());
    apply_config(server, name, &shop_fixture(server.connector), &body).unwrap_or_else(|e| panic!("apply: {e}"))
}

pub fn generate(config: &Path, reset: bool) -> Outcome {
    let opts = GenerateOptions { config_path: config.to_path_buf(), reset, dry_run: false, report: None, progress_bars: false, verify: true };
    run::generate(opts).unwrap_or_else(|e| panic!("generate: {e}"))
}

fn assert_success(outcome: &Outcome) {
    assert!(outcome.error.is_none(), "{:?}\n{}", outcome.error, outcome.verification.as_deref().unwrap_or_default());
}

pub fn connect(config: &Path) -> Database {
    Database::connect(&Config::load(config).unwrap()).unwrap()
}

/// Every generated value of every table, as text, in key order. Columns the database
/// fills (`DEFAULT now()`) differ between loads and are left out.
pub fn dump(config: &Path) -> Vec<Vec<Vec<Option<String>>>> {
    let c = Config::load(config).unwrap();
    let schema = run::load_schema(&c).unwrap();
    let db = Database::connect(&c).unwrap();
    TABLES
        .iter()
        .map(|t| {
            let cols: Vec<String> = schema
                .table(t)
                .unwrap()
                .columns
                .iter()
                .filter(|col| col.default.as_ref().is_none_or(|d| d.kind != DefaultKind::Default))
                .map(|col| db.quote(&col.name))
                .collect();
            db.query_rows(&format!("SELECT {} FROM {} ORDER BY 1, 2", cols.join(", "), db.quote(t))).unwrap()
        })
        .collect()
}

/// The fixture domains: every directory of `tests/fixtures` with a `config.yaml`, sorted.
pub fn domains() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("config.yaml").is_file())
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    out.sort();
    out
}

/// The configuration of a domain, without `database` and `schema`.
fn domain_body(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("config.yaml")).unwrap()
}

/// The shop fixture (if the connector has one) and every domain fixture compile, and the
/// configuration plans against them, without a database.
pub fn fixtures_compile(connector: &str) {
    let mut fixtures: Vec<(String, PathBuf, String)> = Vec::new();
    if shop_fixture(connector).is_file() {
        fixtures.push(("shop".into(), shop_fixture(connector), SHOP.replace("BATCH", "5000").replace("WORKERS", "3")));
    }
    let mut domain_fixtures = 0;
    for (domain, dir) in domains() {
        match dir.join(format!("{connector}.sql")) {
            sql if sql.is_file() => {
                fixtures.push((domain, sql, domain_body(&dir)));
                domain_fixtures += 1;
            }
            _ => eprintln!("{domain}: no {connector}.sql, skipped"),
        }
    }
    assert!(domain_fixtures > 0, "{connector} has no domain fixture: add tests/fixtures/<domain>/{connector}.sql");
    let mut failures = Vec::new();
    for (label, sql, body) in &fixtures {
        let yaml = format!("dialect: {connector}\nschema: \"{}\"\n{body}", sql.display());
        let result = Config::parse(&yaml, Path::new("offline.yaml")).and_then(|c| {
            let schema = run::load_schema(&c)?;
            dbgen::plan::build(&c, &schema, None).map(|_| ())
        });
        if let Err(e) = result {
            failures.push(format!("{label}: {e}"));
        }
    }
    assert!(failures.is_empty(), "fixtures that do not compile in {connector}:\n{}", failures.join("\n"));
}

/// The shop loads, every verification check passes, and the database holds the target.
pub fn end_to_end(server: &Server, name: &str) {
    let config = shop_config(server, name, 5000, 3);
    let outcome = generate(&config, false);
    assert_success(&outcome);
    let summary = outcome.summary.unwrap();
    assert_eq!(summary.tables.iter().map(|t| t.final_rows.unwrap()).sum::<u64>(), TARGET_ROWS);
    let db = connect(&config);
    // The configured domain and the CHECKs hold in the database too.
    assert_eq!(db.query_u64("SELECT COUNT(*) FROM orders WHERE channel NOT IN ('store', 'web', 'app')").unwrap(), 0);
    assert_eq!(db.query_u64("SELECT COUNT(*) FROM orders WHERE channel = 'store' AND store_id IS NULL").unwrap(), 0);
    // The hierarchy points backwards, so it has roots and no cycles.
    assert_eq!(db.query_u64("SELECT COUNT(*) FROM categories WHERE parent_id >= category_id").unwrap(), 0);
    (server.drop)(name);
}

/// The same seed gives the same rows whatever the batch size and the number of workers.
pub fn deterministic(server: &Server, a: &str, b: &str) {
    let ca = shop_config(server, a, 5000, 3);
    let cb = shop_config(server, b, 1777, 2);
    assert_success(&generate(&ca, false));
    assert_success(&generate(&cb, false));
    assert!(dump(&ca) == dump(&cb), "the two loads differ");
    (server.drop)(a);
    (server.drop)(b);
}

/// Rows deleted after a load are put back by the next run, identical.
pub fn resume(server: &Server, name: &str, delete: &str) {
    let config = shop_config(server, name, 4000, 2);
    assert_success(&generate(&config, false));
    let before = dump(&config);
    connect(&config).execute(delete).unwrap();
    let outcome = generate(&config, false);
    assert_success(&outcome);
    let lines = outcome.summary.unwrap().tables.into_iter().find(|t| t.table == "order_lines").unwrap();
    assert_eq!((lines.inserted_rows, lines.final_rows), (50_000, Some(50_000)));
    assert!(dump(&config) == before, "the reloaded rows differ");
    (server.drop)(name);
}

/// A batch whose last row repeats the first one's primary key fails as a whole: no row
/// is left behind, and the error is not worth retrying. The same row alone then loads.
pub fn atomic_batches(server: &Server, name: &str) {
    if !server.atomic_batches {
        eprintln!("{}: no enforced primary key, atomic_batches skipped", server.connector);
        return;
    }
    let config = apply_config(server, name, &shop_fixture(server.connector), SMALL_SHOP).unwrap_or_else(|e| panic!("apply: {e}"));
    assert_success(&generate(&config, false));
    let c = Config::load(&config).unwrap();
    let schema = run::load_schema(&c).unwrap();
    let db = Database::connect(&c).unwrap();
    let table = schema.table("order_lines").unwrap();
    let cols: Vec<usize> = (0..table.columns.len()).filter(|&i| table.columns[i].insertable()).collect();
    let names: Vec<&str> = cols.iter().map(|&i| table.columns[i].name.as_str()).collect();
    let types: Vec<&DataType> = cols.iter().map(|&i| &table.columns[i].ty).collect();
    let positions: Vec<usize> = (0..cols.len()).collect();
    let row = db.read_rows(table, &cols, 1).unwrap().pop().expect("order_lines has rows");
    let batch = |copies: usize| {
        let mut b = Batch::default();
        for _ in 0..copies {
            db.encode_row(&mut b.body, &RowRef { types: &types, values: &row, columns: &positions })
                .unwrap_or_else(|e| panic!("encoding: {}", e.message));
            b.row_ends.push(b.body.len());
        }
        b
    };
    db.truncate(&["order_lines"]).unwrap();
    let e = db.insert(table, &names, &batch(2)).expect_err("a batch with a duplicate primary key must fail");
    assert!(!e.is_transient(), "a duplicate key is deterministic, not transient: {e}");
    assert_eq!(db.count("order_lines").unwrap(), 0, "a failed batch left rows behind");
    db.insert(table, &names, &batch(1)).unwrap();
    assert_eq!(db.count("order_lines").unwrap(), 1);
    (server.drop)(name);
}

/// The first call to a database that does not exist fails with `UnknownDatabase`.
pub fn missing_database(server: &Server, name: &str) {
    if !server.unknown_database {
        eprintln!("{}: databases cannot be missing, missing_database skipped", server.connector);
        return;
    }
    (server.drop)(name);
    let config = Config::parse(&format!("database: \"{}\"", (server.url)(name)), Path::new("dbgen.yaml")).unwrap();
    let e = Database::connect(&config).unwrap().version().expect_err("the database does not exist");
    assert_eq!(e.kind, DbErrorKind::UnknownDatabase, "{e}");
}

/// Every domain fixture of the connector applies, loads every table to its target, and
/// passes every verification check. Failures are collected, so one run reports them all.
pub fn domains_end_to_end(server: &Server, prefix: &str) {
    let mut failures = Vec::new();
    let mut ran = 0;
    for (domain, dir) in domains() {
        let sql = dir.join(format!("{}.sql", server.connector));
        if !sql.is_file() {
            eprintln!("{domain}: no {}.sql, skipped", server.connector);
            continue;
        }
        ran += 1;
        let name = format!("dbgen_it_{prefix}_{domain}_test");
        let config = match apply_config(server, &name, &sql, &domain_body(&dir)) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{domain}: apply: {e}"));
                continue;
            }
        };
        let opts = GenerateOptions { config_path: config, reset: false, dry_run: false, report: None, progress_bars: false, verify: true };
        match run::generate(opts) {
            Err(e) => failures.push(format!("{domain}: generate: {e}")),
            Ok(outcome) if outcome.error.is_some() => {
                failures.push(format!("{domain}: {}\n{}", outcome.error.unwrap(), outcome.verification.unwrap_or_default()));
            }
            Ok(outcome) => {
                let short: Vec<String> = outcome
                    .summary
                    .map(|s| s.tables)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|t| t.final_rows != Some(t.target_rows))
                    .map(|t| format!("{} holds {:?} rows of {}", t.table, t.final_rows, t.target_rows))
                    .collect();
                if short.is_empty() {
                    (server.drop)(&name);
                } else {
                    failures.push(format!("{domain}: {}", short.join(", ")));
                }
            }
        }
    }
    assert!(ran > 0, "{} has no domain fixture: add tests/fixtures/<domain>/{}.sql", server.connector, server.connector);
    assert!(failures.is_empty(), "{} of {ran} domains failed on {}:\n{}", failures.len(), server.connector, failures.join("\n"));
}

/// Every fixture of the connector (the shop and every domain), applied to the database,
/// reads back through `introspect_ddl` as the same schema, and the introspected schema
/// plans against the database exactly as the declared one: same warnings, same repairs
/// in a sample of rows. Failures and differences are collected, so one run reports them
/// all.
pub fn introspection_round_trip(server: &Server, prefix: &str) {
    if !server.introspection {
        eprintln!("{}: no introspection, introspection_round_trip skipped", server.connector);
        return;
    }
    let mut fixtures: Vec<(String, PathBuf, String)> = Vec::new();
    if shop_fixture(server.connector).is_file() {
        fixtures.push(("shop".into(), shop_fixture(server.connector), SHOP.replace("BATCH", "5000").replace("WORKERS", "3")));
    }
    for (domain, dir) in domains() {
        let sql = dir.join(format!("{}.sql", server.connector));
        if sql.is_file() {
            fixtures.push((domain, sql, domain_body(&dir)));
        }
    }
    let mut failures = Vec::new();
    let ran = fixtures.len();
    for (domain, sql, body) in fixtures {
        let name = format!("dbgen_it_{prefix}_{domain}_rt_test");
        match round_trip(server, &name, &sql, &body) {
            Ok(differences) if differences.is_empty() => (server.drop)(&name),
            Ok(differences) => failures.push(format!("{domain}:\n  {}", differences.join("\n  "))),
            Err(e) => failures.push(format!("{domain}: {e}")),
        }
    }
    assert!(ran > 0, "{} has no fixture: add tests/fixtures/<domain>/{}.sql", server.connector, server.connector);
    assert!(failures.is_empty(), "{} of {ran} fixtures do not round-trip on {}:\n{}", failures.len(), server.connector, failures.join("\n"));
}

/// Applies `sql`, introspects the database, and returns the differences between the two
/// schemas, and a failure to plan on the introspected one.
fn round_trip(server: &Server, name: &str, sql: &Path, body: &str) -> Result<Vec<String>, String> {
    let path = apply_config(server, name, sql, body).map_err(|e| format!("apply: {e}"))?;
    let config = Config::load(&path).map_err(|e| e.to_string())?;
    let sources = schema::read_sources(&config.schema, &config.base_dir).map_err(|e| e.to_string())?;
    let declared = schema::compile(config.dialect, &sources).map_err(|e| format!("compiling the fixture: {e}"))?;
    let db = Database::connect(&config).map_err(|e| e.to_string())?;
    let script = db.introspect_ddl().map_err(|e| format!("introspect_ddl: {e}"))?;
    let live = schema::compile(config.dialect, &[Source::new(format!("<{name}>"), script.clone())])
        .map_err(|e| format!("compiling the introspected DDL: {e}\n{script}"))?;
    let mut differences = shape_differences(&declared, &live);
    let rows = Some(&db as &dyn RowSource);
    match (plan::build(&config, &declared, rows), plan::build(&config, &live, rows)) {
        (_, Err(e)) => differences.push(format!("planning on the introspected schema: {e}")),
        (Err(e), _) => differences.push(format!("planning on the declared schema: {e}")),
        (Ok(declared_plan), Ok(live_plan)) => {
            // The same rows need the same repairs: a CHECK whose bounds or values the
            // introspected DDL hides from dbgen shows up here.
            for w in live_plan.warnings.iter().filter(|w| !declared_plan.warnings.contains(w)) {
                differences.push(format!("only on the introspected plan: {w}"));
            }
            let samples = |p: &plan::Plan| -> BTreeMap<String, String> {
                plan::sample(p, 500)
                    .into_iter()
                    .map(|r| (r.table, format!("{} of {} rows repaired, error {:?}", r.repaired, r.sampled, r.error)))
                    .collect()
            };
            let (d, l) = (samples(&declared_plan), samples(&live_plan));
            for (table, sample) in &d {
                if l.get(table) != Some(sample) {
                    differences.push(format!("{table}: sample of the declared plan: {sample}; of the introspected plan: {:?}", l.get(table)));
                }
            }
        }
    }
    if !differences.is_empty() {
        differences.push(format!("introspected DDL:\n{script}"));
    }
    Ok(differences)
}

/// What a table must keep through introspection: its engine, whether it stores rows and
/// which materialized view fills it, columns in order with their types (nullability
/// included), keys as column sets, foreign keys, and how many CHECKs. Constraint names,
/// comments and seeded rows do not round-trip and are left out.
#[derive(Debug, PartialEq)]
struct Shape {
    table: (Option<String>, bool, Option<String>),
    columns: Vec<(String, String)>,
    keys: BTreeSet<(String, BTreeSet<String>)>,
    foreign_keys: BTreeSet<(Vec<String>, String, Vec<String>)>,
    checks: usize,
}

fn shapes(schema: &Schema) -> BTreeMap<String, Shape> {
    let primary_key =
        |table: &str| schema.table(table).and_then(|t| t.primary_key()).map(|k| k.iter().map(|c| c.to_string()).collect()).unwrap_or_default();
    schema
        .tables
        .iter()
        .map(|t| {
            let keys = t
                .keys
                .iter()
                .map(|k| {
                    let parts = k.parts.iter().map(|p| match p {
                        KeyPart::Column(c) => c.clone(),
                        KeyPart::Expr(e) => format!("({e})"),
                    });
                    (format!("{:?}", k.kind), parts.collect())
                })
                .collect();
            let foreign_keys = t
                .foreign_keys
                .iter()
                .map(|fk| {
                    // `REFERENCES parent` without columns means the parent's primary key.
                    let ref_columns = if fk.ref_columns.is_empty() { primary_key(&fk.ref_table) } else { fk.ref_columns.clone() };
                    (fk.columns.clone(), fk.ref_table.clone(), ref_columns)
                })
                .collect();
            let columns = t.columns.iter().map(|c| (c.name.clone(), c.ty.to_string())).collect();
            let table = (t.engine.clone(), t.storage, t.derived_by.clone());
            (t.name.clone(), Shape { table, columns, keys, foreign_keys, checks: t.checks.len() })
        })
        .collect()
}

/// The differences between the schema the fixture declares and the one introspected.
fn shape_differences(declared: &Schema, live: &Schema) -> Vec<String> {
    let (declared, live) = (shapes(declared), shapes(live));
    let mut out = Vec::new();
    for name in declared.keys().chain(live.keys()).collect::<BTreeSet<_>>() {
        let (d, l) = match (declared.get(name), live.get(name)) {
            (Some(d), Some(l)) => (d, l),
            (Some(_), None) => {
                out.push(format!("table {name} is not introspected"));
                continue;
            }
            (None, _) => {
                out.push(format!("table {name} is introspected but not declared"));
                continue;
            }
        };
        if d.table != l.table {
            out.push(format!("{name}: (engine, stores rows, filled by) {:?} declared, {:?} introspected", d.table, l.table));
        }
        if d.columns != l.columns {
            let fmt = |cols: &[(String, String)]| cols.iter().map(|(n, t)| format!("{n} {t}")).collect::<Vec<_>>().join(", ");
            out.push(format!("{name}: columns\n    declared:     {}\n    introspected: {}", fmt(&d.columns), fmt(&l.columns)));
        }
        for k in d.keys.difference(&l.keys) {
            out.push(format!("{name}: key {k:?} is not introspected"));
        }
        for k in l.keys.difference(&d.keys) {
            out.push(format!("{name}: key {k:?} is introspected but not declared"));
        }
        for fk in d.foreign_keys.difference(&l.foreign_keys) {
            out.push(format!("{name}: foreign key {fk:?} is not introspected"));
        }
        for fk in l.foreign_keys.difference(&d.foreign_keys) {
            out.push(format!("{name}: foreign key {fk:?} is introspected but not declared"));
        }
        if d.checks != l.checks {
            out.push(format!("{name}: {} CHECKs declared, {} introspected", d.checks, l.checks));
        }
    }
    out
}
