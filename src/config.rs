//! `dbgen.yaml`: where the DDL is, which database to load, how many rows per table, and
//! optional hints (relations, column domains) the DDL cannot express.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_yaml_ng::Value as Yaml;

use crate::dialect::Dialect;
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    /// Directory of the config file: relative paths are resolved against it.
    pub base_dir: PathBuf,
    /// Database URL, with `${VAR}` expanded.
    pub database: Option<String>,
    pub dialect: Dialect,
    pub schema: Vec<PathBuf>,
    pub generation: Generation,
    pub tables: BTreeMap<String, TableTarget>,
    pub relations: Vec<RelationConfig>,
    pub ignore_relations: Vec<ColumnRef>,
    pub columns: BTreeMap<String, ColumnConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    database: Option<String>,
    dialect: Option<Dialect>,
    #[serde(default)]
    schema: OneOrMany,
    #[serde(default)]
    generation: Generation,
    #[serde(default)]
    tables: BTreeMap<String, Yaml>,
    #[serde(default)]
    relations: Vec<Yaml>,
    #[serde(default)]
    ignore_relations: Vec<String>,
    #[serde(default)]
    columns: BTreeMap<String, ColumnConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    #[default]
    None,
    One(PathBuf),
    Many(Vec<PathBuf>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Generation {
    /// Same seed, schema, targets and window give identical data.
    pub seed: u64,
    /// Rows per INSERT. Each batch is one request; it does not change the data.
    pub batch_size: u64,
    /// Concurrent INSERT connections.
    pub workers: usize,
    /// Tables loaded at the same time.
    pub table_concurrency: usize,
    /// Threads generating and encoding rows. Default: min(workers, CPUs).
    pub generator_threads: Option<usize>,
    /// Encoded batches waiting for a connection. Default: 2 x workers.
    pub queue_depth: Option<usize>,
    /// Length of the generated time window, in days, ending at `end_time`.
    pub history_days: u32,
    /// End of the time window: `YYYY-MM-DD` or a timestamp. Default: today 00:00 UTC.
    pub end_time: Option<String>,
    /// Share of NULLs in nullable columns.
    pub null_rate: f64,
    /// Infer relations from column and table names (`customer_id` -> `customers.id`).
    pub infer_relations: bool,
    /// Generate columns that have a database DEFAULT instead of leaving them to it.
    pub fill_defaults: bool,
    /// Tables of the schema absent from `tables`: `warn` leaves them alone, `error` refuses.
    pub missing_tables: MissingTables,
    /// Attempts per batch on transient failures.
    pub max_retries: u32,
    /// Local-search steps per row to satisfy CHECK constraints.
    pub max_repair_steps: u32,
    /// Rows read from a table that is not generated but is referenced.
    pub max_existing_rows: u64,
    pub http_timeout_secs: u64,
    pub state_file: Option<PathBuf>,
    pub progress_interval_secs: u64,
}

impl Default for Generation {
    fn default() -> Self {
        Generation {
            seed: 42,
            batch_size: 100_000,
            workers: 4,
            table_concurrency: 2,
            generator_threads: None,
            queue_depth: None,
            history_days: 365,
            end_time: None,
            null_rate: 0.1,
            infer_relations: true,
            fill_defaults: false,
            missing_tables: MissingTables::Warn,
            max_retries: 5,
            max_repair_steps: 200,
            max_existing_rows: 1_000_000,
            http_timeout_secs: 300,
            state_file: None,
            progress_interval_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissingTables {
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableTarget {
    /// Generate exactly this many rows.
    Rows(u64),
    /// Never written; its rows are read when other tables reference it.
    Existing,
    /// Never written nor read.
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub table: String,
    pub columns: Vec<String>,
}

impl std::fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.columns.len() == 1 {
            write!(f, "{}.{}", self.table, self.columns[0])
        } else {
            write!(f, "{}.({})", self.table, self.columns.join(", "))
        }
    }
}

impl ColumnRef {
    /// `table.col` or `table.(a, b)`.
    pub fn parse(s: &str) -> Result<ColumnRef, String> {
        let s = s.trim();
        let bad = || format!("'{s}' is not table.column or table.(column, column)");
        if let Some(open) = s.find(".(") {
            let table = s[..open].trim();
            let inner = s[open + 2..].strip_suffix(')').ok_or_else(bad)?;
            let columns: Vec<String> = inner.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
            if table.is_empty() || columns.is_empty() {
                return Err(bad());
            }
            return Ok(ColumnRef { table: table.into(), columns });
        }
        let (table, col) = s.rsplit_once('.').ok_or_else(bad)?;
        if table.is_empty() || col.is_empty() {
            return Err(bad());
        }
        Ok(ColumnRef { table: table.trim().into(), columns: vec![col.trim().into()] })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    /// Every parent row equally likely.
    #[default]
    Uniform,
    /// A few parent rows get most children (power law, `skew` > 1 is steeper).
    Skewed,
}

#[derive(Debug, Clone)]
pub struct RelationConfig {
    pub from: ColumnRef,
    pub to: ColumnRef,
    pub distribution: Distribution,
    pub skew: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRelation {
    from: String,
    to: String,
    #[serde(default)]
    distribution: Distribution,
    skew: Option<f64>,
}

/// Overrides for one column, under `columns: { table.column: {...} }`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnConfig {
    /// Pick from these values (with `weights`, proportionally).
    pub values: Option<Vec<Yaml>>,
    pub weights: Option<Vec<u32>>,
    /// Inclusive bounds: numbers, dates or timestamps.
    pub range: Option<[Yaml; 2]>,
    /// Share of NULLs (nullable columns).
    pub null_rate: Option<f64>,
    /// Always this value (may be null).
    pub value: Option<Yaml>,
    /// Kind of text: first_name, last_name, full_name, email, phone, city, country,
    /// address, postal_code, url, sentence, word, code, uuid, json, ip.
    pub kind: Option<String>,
    /// Make every value distinct.
    pub unique: Option<bool>,
    /// `false`: leave the column to the database (default value or NULL).
    pub generate: Option<bool>,
    /// A timestamp that follows another column of the row, by `gap`.
    pub after: Option<String>,
    /// `[min, max]` duration after `after`, e.g. `[1m, 2h]`.
    pub gap: Option<[String; 2]>,
    /// `[min, max]` elements of an array or map, or words of a sentence.
    pub length: Option<[u32; 2]>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Config::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let raw: RawConfig = serde_yaml_ng::from_str(text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let base_dir = if base_dir.as_os_str().is_empty() { PathBuf::from(".") } else { base_dir };
        let database = raw.database.as_deref().map(expand_env).transpose()?;
        let dialect = match (raw.dialect, database.as_deref()) {
            (Some(d), _) => d,
            (None, Some(url)) => dialect_of_url(url)?,
            (None, None) => Dialect::Clickhouse,
        };
        let schema = match raw.schema {
            OneOrMany::None => Vec::new(),
            OneOrMany::One(p) => vec![p],
            OneOrMany::Many(v) => v,
        };
        let g = &raw.generation;
        if g.batch_size == 0 || g.workers == 0 || g.table_concurrency == 0 {
            return Err(Error::Config("generation: batch_size, workers and table_concurrency must be positive".into()));
        }
        if !(0.0..=1.0).contains(&g.null_rate) {
            return Err(Error::Config("generation.null_rate must be between 0 and 1".into()));
        }
        if g.history_days == 0 {
            return Err(Error::Config("generation.history_days must be positive".into()));
        }
        let mut tables = BTreeMap::new();
        for (name, v) in raw.tables {
            let target = match &v {
                Yaml::Number(n) => TableTarget::Rows(n.as_u64().ok_or_else(|| Error::Config(format!("tables.{name}: expected a row count, got {n}")))?),
                Yaml::String(s) if s.eq_ignore_ascii_case("existing") => TableTarget::Existing,
                Yaml::String(s) if s.eq_ignore_ascii_case("skip") => TableTarget::Skip,
                Yaml::Null => TableTarget::Skip,
                other => {
                    return Err(Error::Config(format!("tables.{name}: expected a row count, 'existing' or 'skip', got {}", yaml_str(other))))
                }
            };
            tables.insert(name, target);
        }
        let mut relations = Vec::new();
        for (i, r) in raw.relations.into_iter().enumerate() {
            let rel = match r {
                Yaml::String(s) => {
                    let (from, to) = s.split_once("->").ok_or_else(|| Error::Config(format!("relations[{i}]: '{s}' is not 'child.col -> parent.col'")))?;
                    RelationConfig { from: parse_ref(from, i)?, to: parse_ref(to, i)?, distribution: Distribution::Uniform, skew: 1.5 }
                }
                other => {
                    let raw: RawRelation = serde_yaml_ng::from_value(other).map_err(|e| Error::Config(format!("relations[{i}]: {e}")))?;
                    RelationConfig { from: parse_ref(&raw.from, i)?, to: parse_ref(&raw.to, i)?, distribution: raw.distribution, skew: raw.skew.unwrap_or(1.5) }
                }
            };
            if rel.from.columns.len() != rel.to.columns.len() {
                return Err(Error::Config(format!("relations[{i}]: {} and {} have different numbers of columns", rel.from, rel.to)));
            }
            if rel.skew <= 0.0 {
                return Err(Error::Config(format!("relations[{i}]: skew must be positive")));
            }
            relations.push(rel);
        }
        let ignore_relations = raw.ignore_relations.iter().map(|s| ColumnRef::parse(s).map_err(|e| Error::Config(format!("ignore_relations: {e}")))).collect::<Result<_>>()?;
        for (k, c) in &raw.columns {
            ColumnRef::parse(k).map_err(|e| Error::Config(format!("columns: {e}")))?;
            if let (Some(v), Some(w)) = (&c.values, &c.weights) {
                if v.len() != w.len() {
                    return Err(Error::Config(format!("columns.{k}: values and weights have different lengths")));
                }
            }
            if c.null_rate.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
                return Err(Error::Config(format!("columns.{k}.null_rate must be between 0 and 1")));
            }
        }
        Ok(Config {
            path: path.to_path_buf(),
            base_dir,
            database,
            dialect,
            schema,
            generation: raw.generation,
            tables,
            relations,
            ignore_relations,
            columns: raw.columns,
        })
    }

    /// The configuration of `table.column`, if any.
    pub fn column(&self, table: &str, column: &str) -> Option<&ColumnConfig> {
        self.columns.get(&format!("{table}.{column}"))
    }

    pub fn database_url(&self) -> Result<&str> {
        self.database.as_deref().ok_or_else(|| Error::Config(format!("{}: 'database' is not set", self.path.display())))
    }

    pub fn generator_threads(&self) -> usize {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        self.generation.generator_threads.unwrap_or_else(|| self.generation.workers.min(cpus)).max(1)
    }

    pub fn queue_depth(&self) -> usize {
        self.generation.queue_depth.unwrap_or(2 * self.generation.workers).max(1)
    }
}

fn parse_ref(s: &str, i: usize) -> Result<ColumnRef> {
    ColumnRef::parse(s).map_err(|e| Error::Config(format!("relations[{i}]: {e}")))
}

pub fn yaml_str(v: &Yaml) -> String {
    serde_yaml_ng::to_string(v).map(|s| s.trim().to_string()).unwrap_or_else(|_| format!("{v:?}"))
}

fn dialect_of_url(url: &str) -> Result<Dialect> {
    let scheme = url.split("://").next().unwrap_or_default().to_ascii_lowercase();
    match scheme.as_str() {
        "clickhouse" | "ch" | "http" | "https" => Ok(Dialect::Clickhouse),
        other => Err(Error::Config(format!("database: no supported dialect for '{other}://' URLs (supported: clickhouse://, http(s)://)"))),
    }
}

/// Expands `${VAR}` from the environment.
pub fn expand_env(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| Error::Config(format!("unterminated ${{ in '{s}'")))?;
        let name = &after[..end];
        let value = std::env::var(name).map_err(|_| Error::Config(format!("environment variable {name} is not set")))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let c = Config::parse(
            r#"
database: "http://u:p@localhost:8123/db"
schema: [migrations, extra.sql]
generation: { seed: 7, batch_size: 5000 }
tables:
  orders: 1000
  countries: existing
  audit: skip
relations:
  - "orders.customer_id -> customers.id"
  - { from: "lines.(order_id, n)", to: "order_slots.(order_id, n)", distribution: skewed, skew: 2 }
ignore_relations: ["orders.parent_id"]
columns:
  orders.status: { values: [NEW, PAID], weights: [3, 1] }
  orders.paid_at: { after: created_at, gap: [1m, 2d], null_rate: 0.3 }
"#,
            Path::new("cfg/dbgen.yaml"),
        )
        .unwrap();
        assert_eq!(c.base_dir, PathBuf::from("cfg"));
        assert_eq!(c.schema.len(), 2);
        assert_eq!(c.generation.seed, 7);
        assert_eq!(c.generation.workers, 4);
        assert_eq!(c.tables["orders"], TableTarget::Rows(1000));
        assert_eq!(c.tables["countries"], TableTarget::Existing);
        assert_eq!(c.relations[1].from.columns, ["order_id", "n"]);
        assert_eq!(c.relations[1].distribution, Distribution::Skewed);
        assert_eq!(c.column("orders", "paid_at").unwrap().after.as_deref(), Some("created_at"));
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(Config::parse("generation: { sed: 1 }", Path::new("x.yaml")).is_err());
        assert!(Config::parse("tables: { a: lots }", Path::new("x.yaml")).is_err());
        assert!(Config::parse("relations: ['a.b c.d']", Path::new("x.yaml")).is_err());
        assert!(Config::parse("database: 'postgres://x/y'", Path::new("x.yaml")).is_err());
    }

    #[test]
    fn expands_environment() {
        std::env::set_var("DBGEN_TEST_PW", "s3cret");
        assert_eq!(expand_env("http://u:${DBGEN_TEST_PW}@h/db").unwrap(), "http://u:s3cret@h/db");
        assert!(expand_env("${DBGEN_SURELY_UNSET_VAR}").is_err());
    }
}
