//! Integration tests against a real ClickHouse server.
//!
//! Set `DBGEN_TEST_CLICKHOUSE_URL` to the server without a database, e.g.
//! `http://dbgen:dbgen@localhost:18123` (`docker compose up -d` starts one). The tests
//! create, fill and drop databases whose names end in `_test`, and touch nothing else.
//! Without the variable they pass without doing anything.

use std::path::{Path, PathBuf};

use dbgen::config::Config;
use dbgen::db::clickhouse::{encode, Client};
use dbgen::dialect::Dialect;
use dbgen::plan;
use dbgen::run::{self, GenerateOptions};
use dbgen::schema::{self, DataType};
use dbgen::value::{parse_uuid, Value};

fn server() -> Option<String> {
    match std::env::var("DBGEN_TEST_CLICKHOUSE_URL") {
        Ok(u) if !u.is_empty() => Some(u.trim_end_matches('/').to_string()),
        _ => {
            eprintln!("DBGEN_TEST_CLICKHOUSE_URL not set: skipping");
            None
        }
    }
}

fn admin(base: &str) -> Client {
    Client::connect(&format!("{base}/default"), 60).expect("client")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A fresh database and a configuration for the shop fixture pointing at it.
fn shop_config(base: &str, db: &str, batch_size: u64, workers: usize) -> PathBuf {
    assert!(db.ends_with("_test"), "tests only touch *_test databases");
    admin(base).execute(&format!("DROP DATABASE IF EXISTS {db} SYNC")).expect("drop");
    let dir = std::env::temp_dir().join(format!("dbgen-it-{}-{db}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yaml = format!(
        r#"
database: "{base}/{db}"
schema: "{schema}"
generation: {{ seed: 7, batch_size: {batch_size}, workers: {workers}, history_days: 90, end_time: "2026-01-01" }}
tables:
  customers: 2000
  products: 300
  orders: 20000
  order_lines: 50000
  countries: existing
relations:
  - "customers.country_code -> countries.code"
columns:
  orders.channel: {{ values: [store, web, app], weights: [2, 5, 3] }}
  orders.status: {{ values: [new, paid, shipped, cancelled], weights: [1, 3, 5, 1] }}
"#,
        schema = fixture("shop.sql").display()
    );
    let path = dir.join("dbgen.yaml");
    std::fs::write(&path, yaml).unwrap();
    run::apply(&path, true).expect("apply");
    path
}

fn generate(config: &Path, reset: bool) -> run::Outcome {
    run::generate(GenerateOptions { config_path: config.to_path_buf(), reset, dry_run: false, report: None, progress_bars: false, verify: true })
        .expect("generate")
}

fn checksums(base: &str, db: &str) -> Vec<(String, String)> {
    let c = Client::connect(&format!("{base}/{db}"), 60).unwrap();
    ["customers", "products", "orders", "order_lines"]
        .iter()
        .map(|t| (t.to_string(), c.query_text(&format!("SELECT count(), groupBitXor(cityHash64(*)) FROM {t} FORMAT TSV")).unwrap()))
        .collect()
}

#[test]
fn encoder_matches_the_server() {
    let Some(base) = server() else { return };
    let c = admin(&base);
    let dt = |s: &str| Dialect::Clickhouse.parse_type(s).unwrap();
    let cases: Vec<(&str, &str, Value)> = vec![
        ("Int8", "-5", Value::Int(-5)),
        ("UInt64", "'18446744073709551615'", Value::Int(u64::MAX as i128)),
        ("Int128", "'-170141183460469231731687303715884105728'", Value::Int(i128::MIN)),
        ("Int256", "'-12345678901234567890'", Value::Int(-12_345_678_901_234_567_890)),
        ("UInt256", "'12345678901234567890'", Value::Int(12_345_678_901_234_567_890)),
        ("Float32", "1.5", Value::Float(1.5)),
        ("Float64", "-0.25", Value::Float(-0.25)),
        ("Decimal(4, 3)", "'1.234'", Value::Decimal(1234, 3)),
        ("Decimal(18, 2)", "'-99.95'", Value::Decimal(-9995, 2)),
        ("Decimal(38, 5)", "'12345.6789'", Value::Decimal(1_234_567_890, 5)),
        ("Decimal(76, 2)", "'-1.5'", Value::Decimal(-150, 2)),
        ("String", "'héllo'", Value::Str("héllo".into())),
        ("FixedString(4)", "'ab'", Value::Str("ab".into())),
        ("UUID", "'61f0c404-5cb3-11e7-907b-a6006ad3dba0'", Value::Uuid(parse_uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0").unwrap())),
        ("Date", "'2024-02-29'", Value::Date(19_782)),
        ("Date32", "'1950-01-01'", Value::Date(-7305)),
        ("DateTime('UTC')", "'2024-01-01 12:00:00'", Value::Time(1_704_110_400_000_000)),
        ("DateTime64(3, 'UTC')", "'2024-01-01 12:00:00.123'", Value::Time(1_704_110_400_123_000)),
        ("DateTime64(9, 'UTC')", "'2024-01-01 12:00:00.123456'", Value::Time(1_704_110_400_123_456)),
        ("Enum8('a' = 1, 'b' = -2)", "'b'", Value::Str("b".into())),
        ("Enum16('x' = 1000)", "'x'", Value::Str("x".into())),
        ("Bool", "true", Value::Bool(true)),
        ("IPv4", "'1.2.3.4'", Value::Ip4(0x0102_0304)),
        ("IPv6", "'::1'", Value::Ip6(1)),
        ("Array(Nullable(Int32))", "[1, NULL]", Value::Array(vec![Value::Int(1), Value::Null])),
        ("Map(String, UInt8)", "map('a', 1)", Value::Map(vec![(Value::Str("a".into()), Value::Int(1))])),
        ("Tuple(s String, n UInt8)", "tuple('x', 3)", Value::Tuple(vec![Value::Str("x".into()), Value::Int(3)])),
        ("LowCardinality(Nullable(String))", "'q'", Value::Str("q".into())),
        ("Nullable(String)", "NULL", Value::Null),
    ];
    for (ty, lit, value) in cases {
        let server = c.query_bytes(&format!("SELECT CAST({lit} AS {ty}) FORMAT RowBinary")).unwrap_or_else(|e| panic!("{ty}: {e}"));
        let mut ours = Vec::new();
        encode(&mut ours, &dt(ty), &value).unwrap_or_else(|e| panic!("{ty}: {e}"));
        assert_eq!(ours, server, "{ty} {lit}");
    }
}

#[test]
fn shop_end_to_end() {
    let Some(base) = server() else { return };
    let config = shop_config(&base, "dbgen_it_shop_test", 5000, 2);
    let outcome = generate(&config, false);
    assert!(outcome.error.is_none(), "{:?}\n{}", outcome.error, outcome.verification.unwrap_or_default());
    let summary = outcome.summary.unwrap();
    assert_eq!(summary.tables.iter().map(|t| t.final_rows.unwrap()).sum::<u64>(), 72_300);
    // The materialized view filled its target from the generated rows.
    let c = Client::connect(&format!("{base}/dbgen_it_shop_test"), 60).unwrap();
    assert!(c.query_u64("SELECT count() FROM daily_sales").unwrap() > 0);
    // Orders keep to their CHECKs and configured domains.
    assert_eq!(c.query_u64("SELECT countIf(channel NOT IN ('store', 'web', 'app')) FROM orders").unwrap(), 0);
    assert_eq!(c.query_u64("SELECT countIf(channel = 'store' AND store_id IS NULL) FROM orders").unwrap(), 0);
    assert_eq!(c.query_u64("SELECT countIf(discount + tax + net_share != 1) FROM products").unwrap(), 0);
    admin(&base).execute("DROP DATABASE IF EXISTS dbgen_it_shop_test SYNC").unwrap();
}

#[test]
fn same_seed_same_data_for_any_batching() {
    let Some(base) = server() else { return };
    let a = shop_config(&base, "dbgen_it_det_a_test", 5000, 2);
    let b = shop_config(&base, "dbgen_it_det_b_test", 1777, 3);
    assert!(generate(&a, false).error.is_none());
    assert!(generate(&b, false).error.is_none());
    assert_eq!(checksums(&base, "dbgen_it_det_a_test"), checksums(&base, "dbgen_it_det_b_test"));
    for db in ["dbgen_it_det_a_test", "dbgen_it_det_b_test"] {
        admin(&base).execute(&format!("DROP DATABASE IF EXISTS {db} SYNC")).unwrap();
    }
}

#[test]
fn resume_reloads_a_table_that_lost_rows() {
    let Some(base) = server() else { return };
    let config = shop_config(&base, "dbgen_it_resume_test", 4000, 2);
    assert!(generate(&config, false).error.is_none());
    let before = checksums(&base, "dbgen_it_resume_test");
    let c = Client::connect(&format!("{base}/dbgen_it_resume_test"), 60).unwrap();
    c.execute("TRUNCATE TABLE orders").unwrap();
    let outcome = generate(&config, false);
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let orders = outcome.summary.unwrap().tables.into_iter().find(|t| t.table == "orders").unwrap();
    assert_eq!((orders.inserted_rows, orders.final_rows), (20_000, Some(20_000)));
    assert_eq!(checksums(&base, "dbgen_it_resume_test"), before);
    admin(&base).execute("DROP DATABASE IF EXISTS dbgen_it_resume_test SYNC").unwrap();
}

/// The planner on a schema it knows nothing about, without a database.
#[test]
fn plan_for_an_unknown_schema() {
    let config = Config::load(&fixture("shop.yaml")).unwrap();
    let sources = schema::read_sources(&config.schema, &config.base_dir).unwrap();
    let schema = schema::compile(Dialect::Clickhouse, &sources).unwrap();
    assert_eq!(schema.tables.len(), 6);
    assert!(schema.table("countries").unwrap().seeded);
    assert_eq!(schema.table("daily_sales").unwrap().derived_by.as_deref(), Some("daily_sales_mv"));
    assert_eq!(schema.table("customers").unwrap().columns[2].name, "phone", "ALTER ... AFTER email");
    assert_eq!(schema.table("orders").unwrap().column("channel").unwrap().ty, DataType::LowCardinality(Box::new(DataType::String { max_len: None })));
    let plan = plan::build(&config, &schema, None).unwrap();
    let describe = |t: &str, c: &str| {
        let ti = schema.table_index(t).unwrap();
        let tg = plan.world.table(ti);
        tg.cols[schema.tables[ti].column_index(c).unwrap()].describe.clone()
    };
    assert!(describe("customers", "is_active").contains("computed from other columns"));
    assert!(describe("customers", "closed_at").starts_with("after signup_at"));
    assert!(describe("customers", "customer_id").starts_with("unique sequence"));
    assert!(describe("products", "net_share").contains("computed from other columns"), "{}", describe("products", "net_share"));
    assert!(describe("orders", "customer_id").contains("references customers.customer_id"));
    assert!(describe("orders", "placed_at").contains("partition key"));
    assert!(describe("order_lines", "order_id").contains("combinations without repeats"));
    assert!(describe("order_lines", "product_id").contains("combinations without repeats"));
    assert!(describe("order_lines", "quantity").contains("[1, 20]"));
    let samples = plan::sample(&plan, 2000);
    for s in samples {
        assert!(s.error.is_none(), "{}: {:?}", s.table, s.error);
    }
}
