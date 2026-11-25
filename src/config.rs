//! `dbgen.yaml`: where the DDL is, which database to load, how many rows per table, and
//! optional hints (relations, column domains) the DDL cannot express.
//!
//! The file is YAML, so JSON works too. Every setting can also come from the command line
//! ([`Overrides`]), which takes precedence; without a file, the command line is the whole
//! configuration. Without `schema`, the schema is read from the database.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_yaml_ng::Value as Yaml;

use crate::dialect::Dialect;
use crate::error::{Error, Result};

/// A loaded `dbgen.yaml`, validated as far as it can be without the schema.
#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    /// Directory of the config file: relative paths are resolved against it.
    pub base_dir: PathBuf,
    /// Database URL, with `${VAR}` expanded.
    pub database: Option<String>,
    /// SQL dialect of the DDL, which is also the connector of the database loaded: the
    /// `dialect` key, or the connector of the database URL's scheme.
    pub dialect: Dialect,
    /// DDL files and directories, in application order.
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

/// The `generation` section: seed, time window and load tuning.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Generation {
    /// Same seed, schema, targets and window give identical data.
    pub seed: u64,
    /// Rows per batch. Each batch is inserted atomically; the size does not change the data.
    pub batch_size: u64,
    /// Concurrent writing connections (SQLite has one writer whatever this says).
    pub workers: usize,
    /// Tables of one dependency level loaded at the same time.
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
    /// Rows for every table that `tables` does not list (instead of leaving it alone).
    /// Tables the DDL seeds, and targets of materialized views, are still left alone.
    pub default_rows: Option<u64>,
    /// Attempts per batch on transient failures.
    pub max_retries: u32,
    /// Local-search steps per row to satisfy CHECK constraints.
    pub max_repair_steps: u32,
    /// Distinct rows read from each table that is not generated but is referenced.
    pub max_existing_rows: u64,
    /// Timeout of one HTTP request (ClickHouse).
    pub http_timeout_secs: u64,
    /// Where the resume state is kept. Default: `.dbgen/<target>.json` next to the config.
    pub state_file: Option<PathBuf>,
    /// Interval between progress log lines, when progress bars are not shown.
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
            default_rows: None,
            max_retries: 5,
            max_repair_steps: 200,
            max_existing_rows: 1_000_000,
            http_timeout_secs: 300,
            state_file: None,
            progress_interval_secs: 10,
        }
    }
}

/// What to do with tables of the schema that `tables` does not mention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissingTables {
    Warn,
    Error,
}

/// What to do with one table, from `tables`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableTarget {
    /// Generate exactly this many rows.
    Rows(u64),
    /// Never written; its rows are read when other tables reference it.
    Existing,
    /// Never written nor read.
    Skip,
}

/// One or more columns of a table: `table.col` or `table.(a, b)`.
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

/// How child rows spread over the parent rows they reference.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    /// Every parent row equally likely.
    #[default]
    Uniform,
    /// A few parent rows get most children (power law, `skew` > 1 is steeper).
    Skewed,
}

/// A configured relation: `from` holds keys of `to`.
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
    /// `[min, max]` elements of an array or map.
    pub length: Option<[u32; 2]>,
}

/// What [`Config::resolve`] calls the configuration when there is no file.
const COMMAND_LINE: &str = "<command line>";

impl Config {
    /// Reads and validates a configuration file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Config::parse(&text, path)
    }

    /// The configuration of a command: `file` (if any) with `overrides` applied over it.
    /// Without a file, the overrides are the whole configuration, and relative paths in it
    /// are relative to the current directory.
    pub fn resolve(file: Option<&Path>, overrides: &Overrides) -> Result<Config> {
        match file {
            // Parsed straight from the text, so errors keep their line numbers.
            Some(path) if overrides.is_empty() => Config::load(path),
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
                let mut doc: Yaml = serde_yaml_ng::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                overrides.apply(&mut doc)?;
                Config::from_document(doc, path)
            }
            None => {
                let mut doc = Yaml::Mapping(Default::default());
                overrides.apply(&mut doc)?;
                Config::from_document(doc, Path::new(COMMAND_LINE))
            }
        }
    }

    fn from_document(doc: Yaml, path: &Path) -> Result<Config> {
        let raw: RawConfig = serde_yaml_ng::from_value(doc).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Config::from_raw(raw, path)
    }

    /// Parses a configuration; `path` locates relative paths and names the file in messages.
    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let raw: RawConfig = serde_yaml_ng::from_str(text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Config::from_raw(raw, path)
    }

    fn from_raw(raw: RawConfig, path: &Path) -> Result<Config> {
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let base_dir = if base_dir.as_os_str().is_empty() { PathBuf::from(".") } else { base_dir };
        let database = raw.database.as_deref().map(expand_env).transpose()?;
        let dialect = match (raw.dialect, database.as_deref()) {
            (Some(d), _) => d,
            (None, Some(url)) => dialect_of_url(url)?,
            (None, None) => {
                return Err(Error::Config(format!(
                    "{}: no database: set `database` (--database), or `dialect` (--dialect) to work without one",
                    path.display()
                )))
            }
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
            if let (Some(v), Some(w)) = (&c.values, &c.weights)
                && v.len() != w.len() {
                    return Err(Error::Config(format!("columns.{k}: values and weights have different lengths")));
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

    /// The database URL, or an error saying it is missing.
    pub fn database_url(&self) -> Result<&str> {
        self.database.as_deref().ok_or_else(|| Error::Config(format!("{}: no database: set `database` (--database)", self.path.display())))
    }

    /// Whether the configuration came from the command line alone, without a file.
    pub fn is_command_line_only(&self) -> bool {
        self.path == Path::new(COMMAND_LINE)
    }

    /// Threads generating rows: configured, or one per worker up to the CPUs.
    pub fn generator_threads(&self) -> usize {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        self.generation.generator_threads.unwrap_or_else(|| self.generation.workers.min(cpus)).max(1)
    }

    /// Encoded batches that may wait for a writer: configured, or two per worker.
    pub fn queue_depth(&self) -> usize {
        self.generation.queue_depth.unwrap_or(2 * self.generation.workers).max(1)
    }
}

/// Settings given on the command line. They take precedence over the configuration file,
/// key by key: `--rows orders=10` changes the target of `orders` and keeps the others.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// `database`.
    pub database: Option<String>,
    /// `dialect`, by name.
    pub dialect: Option<String>,
    /// `schema`: replaces the file's list when not empty.
    pub schema: Vec<PathBuf>,
    /// Entries of `tables`, each `TABLE=N`, `TABLE=existing` or `TABLE=skip`.
    pub rows: Vec<String>,
    /// Keys of `generation`, with their values.
    pub generation: Vec<(&'static str, Yaml)>,
}

impl Overrides {
    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.database.is_none() && self.dialect.is_none() && self.schema.is_empty() && self.rows.is_empty() && self.generation.is_empty()
    }

    /// Sets `generation.<key>` when `value` is given.
    pub fn set<T: Into<Yaml>>(&mut self, key: &'static str, value: Option<T>) {
        if let Some(v) = value {
            self.generation.push((key, v.into()));
        }
    }

    /// Sets `generation.<key>` to a path given on the command line, made absolute so it
    /// does not depend on where the configuration file is.
    pub fn set_path(&mut self, key: &'static str, value: Option<&Path>) -> Result<()> {
        if let Some(p) = value {
            self.generation.push((key, Yaml::String(absolute(p)?.display().to_string())));
        }
        Ok(())
    }

    /// Writes the overrides into a parsed configuration document.
    fn apply(&self, doc: &mut Yaml) -> Result<()> {
        if doc.is_null() {
            *doc = Yaml::Mapping(Default::default());
        }
        let Yaml::Mapping(map) = doc else {
            return Err(Error::Config("the configuration file must be a mapping of settings".into()));
        };
        if let Some(url) = &self.database {
            map.insert("database".into(), Yaml::String(url.clone()));
        }
        if let Some(d) = &self.dialect {
            map.insert("dialect".into(), Yaml::String(d.clone()));
        }
        if !self.schema.is_empty() {
            let paths = self.schema.iter().map(|p| absolute(p).map(|p| Yaml::String(p.display().to_string()))).collect::<Result<Vec<_>>>()?;
            map.insert("schema".into(), Yaml::Sequence(paths));
        }
        if !self.rows.is_empty() {
            let tables = section(map, "tables")?;
            for spec in &self.rows {
                let (table, target) = spec
                    .split_once('=')
                    .map(|(t, v)| (t.trim(), v.trim()))
                    .filter(|(t, v)| !t.is_empty() && !v.is_empty())
                    .ok_or_else(|| Error::Config(format!("--rows: '{spec}' is not TABLE=N, TABLE=existing or TABLE=skip")))?;
                let value = match target.parse::<u64>() {
                    Ok(n) => Yaml::Number(n.into()),
                    Err(_) => Yaml::String(target.to_string()),
                };
                tables.insert(Yaml::String(table.to_string()), value);
            }
        }
        if !self.generation.is_empty() {
            let generation = section(map, "generation")?;
            for (key, value) in &self.generation {
                generation.insert(Yaml::String((*key).to_string()), value.clone());
            }
        }
        Ok(())
    }
}

/// The mapping under `key`, created if absent.
fn section<'a>(map: &'a mut serde_yaml_ng::Mapping, key: &str) -> Result<&'a mut serde_yaml_ng::Mapping> {
    let entry = map.entry(Yaml::String(key.to_string())).or_insert_with(|| Yaml::Mapping(Default::default()));
    if entry.is_null() {
        *entry = Yaml::Mapping(Default::default());
    }
    entry.as_mapping_mut().ok_or_else(|| Error::Config(format!("`{key}` in the configuration file must be a mapping")))
}

/// `p` made absolute against the current directory, with `.` and `..` removed lexically.
fn absolute(p: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| Error::Config(format!("current directory: {e}")))?.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn parse_ref(s: &str, i: usize) -> Result<ColumnRef> {
    ColumnRef::parse(s).map_err(|e| Error::Config(format!("relations[{i}]: {e}")))
}

/// A YAML value as it would be written in the file, for messages.
pub fn yaml_str(v: &Yaml) -> String {
    serde_yaml_ng::to_string(v).map(|s| s.trim().to_string()).unwrap_or_else(|_| format!("{v:?}"))
}

/// The connector a URL points at, from its scheme.
fn dialect_of_url(url: &str) -> Result<Dialect> {
    let scheme = url.split(':').next().unwrap_or_default();
    Dialect::from_scheme(scheme).ok_or_else(|| {
        Error::Config(format!(
            "database: '{scheme}:' URLs are not supported; this build knows {}",
            Dialect::known_schemes()
        ))
    })
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
        assert!(Config::parse("database: 'oracle://x/y'", Path::new("x.yaml")).is_err());
    }

    #[test]
    fn expands_environment() {
        // SAFETY: std serializes its own environment accesses, and nothing in this test
        // binary reads the environment through C.
        unsafe { std::env::set_var("DBGEN_TEST_PW", "s3cret") };
        assert_eq!(expand_env("http://u:${DBGEN_TEST_PW}@h/db").unwrap(), "http://u:s3cret@h/db");
        assert!(expand_env("${DBGEN_SURELY_UNSET_VAR}").is_err());
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    fn file(text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("dbgen-override-{}-{}.yaml", std::process::id(), text.len()));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn flags_alone_make_a_configuration() {
        let o = Overrides { database: Some("sqlite:x.db".into()), rows: vec!["a=5".into(), "b=existing".into()], ..Overrides::default() };
        let c = Config::resolve(None, &o).unwrap();
        assert!(c.is_command_line_only());
        assert_eq!(c.dialect.name(), "sqlite");
        assert_eq!((c.tables["a"].clone(), c.tables["b"].clone()), (TableTarget::Rows(5), TableTarget::Existing));
        assert!(c.schema.is_empty(), "no schema: read from the database");
    }

    #[test]
    fn flags_override_the_file_key_by_key() {
        let path = file("database: 'sqlite:f.db'\ngeneration: { seed: 1, workers: 2 }\ntables: { a: 1, b: 2 }\n");
        let mut o = Overrides { rows: vec!["b=20".into(), "c=skip".into()], schema: vec!["ddl/x.sql".into()], ..Overrides::default() };
        o.set("seed", Some(9u64));
        let c = Config::resolve(Some(&path), &o).unwrap();
        assert_eq!((c.generation.seed, c.generation.workers), (9, 2));
        assert_eq!((c.tables["a"].clone(), c.tables["b"].clone(), c.tables["c"].clone()), (TableTarget::Rows(1), TableTarget::Rows(20), TableTarget::Skip));
        assert!(c.schema[0].is_absolute() && c.schema[0].ends_with("ddl/x.sql"), "command-line paths are relative to the current directory");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn json_files_and_bad_flags() {
        let path = file(r#"{"dialect": "postgres", "tables": {"t": 3}}"#);
        assert_eq!(Config::resolve(Some(&path), &Overrides::default()).unwrap().tables["t"], TableTarget::Rows(3));
        let bad = Overrides { dialect: Some("postgres".into()), rows: vec!["t".into()], ..Overrides::default() };
        assert!(Config::resolve(None, &bad).unwrap_err().to_string().contains("TABLE=N"));
        let mut bad = Overrides { dialect: Some("postgres".into()), ..Overrides::default() };
        bad.set("workers", Some(0u64));
        assert!(Config::resolve(None, &bad).is_err(), "flags go through the same validation as the file");
        let _ = std::fs::remove_file(path);
    }
}
