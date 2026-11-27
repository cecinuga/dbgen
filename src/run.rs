//! The commands: inspect, init, validate, apply, generate, verify.
//!
//! Each takes a resolved [`Config`] (file and command-line flags merged). The schema comes
//! from the configured DDL files, or, when there are none, from the database itself
//! ([`Backend::introspect_ddl`](crate::db::Backend::introspect_ddl)).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::Database;
use crate::engine::{self, Load, Stats};
use crate::error::{Error, Result};
use crate::plan::{self, Plan};
use crate::progress::Progress;
use crate::report::{OverallStatus, Summary};
use crate::schema::{self, KeyPart, Schema};
use crate::sql::Source;
use crate::state::{self, State};
use crate::timeutil::Rfc3339;
use crate::verify;

/// The schema of the configuration: compiled from its DDL files, or read from the
/// database when it has none. Logs the schema's warnings.
pub fn load_schema(config: &Config) -> Result<Schema> {
    if config.schema.is_empty() {
        let db = Database::connect(config)?;
        return schema_from_database(config, &db);
    }
    schema_from_files(config)
}

/// Like [`load_schema`], reusing a connected database when the schema is read from it.
fn load_schema_with(config: &Config, db: &Database) -> Result<Schema> {
    if config.schema.is_empty() { schema_from_database(config, db) } else { schema_from_files(config) }
}

fn schema_from_files(config: &Config) -> Result<Schema> {
    let sources = schema::read_sources(&config.schema, &config.base_dir)?;
    let schema = schema::compile(config.dialect, &sources)?;
    log_warnings(&schema);
    Ok(schema)
}

/// Asks the database for its DDL and compiles it as if it were a file.
fn schema_from_database(config: &Config, db: &Database) -> Result<Schema> {
    let at = format!("connecting to {}", db.describe());
    db.version().map_err(|e| e.into_error(&at))?;
    let ddl = db.introspect_ddl().map_err(|e| e.into_error("reading the schema from the database"))?;
    let schema = schema::compile(config.dialect, &[Source::new(format!("<{}>", db.describe()), ddl)])?;
    tracing::info!(tables = schema.tables.len(), "schema read from the database");
    log_warnings(&schema);
    Ok(schema)
}

fn log_warnings(schema: &Schema) {
    for w in &schema.warnings {
        tracing::warn!("{w}");
    }
}

/// `dbgen inspect`: the schema, and the plan when there are row targets. Needs the
/// database only when the schema is read from it.
/// With `out`, also writes the schema's DDL there as one SQL script ([`ddl_script`]),
/// creating missing directories and replacing an existing file.
pub fn inspect(config: &Config, json: bool, out: Option<&Path>) -> Result<()> {
    let schema = load_schema(config)?;
    if let Some(path) = out {
        write_file(path, &ddl_script(&schema))?;
        eprintln!("Wrote the DDL ({} statements) to {}", schema.statements.len(), path.display());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&schema).expect("serializable"));
        return Ok(());
    }
    println!("{}", render_schema(&schema));
    if !config.tables.is_empty() || config.generation.default_rows.is_some() {
        let plan = plan::build(config, &schema, None)?;
        println!("{}", plan.render(&schema));
        println!("(Without a database: rows of tables that are referenced but not generated are not read. `dbgen validate` reads them.)");
    }
    Ok(())
}

/// The DDL of a schema as one SQL script: every statement it was compiled from, in order
/// and as written, each ending with `;`, under a comment naming the file it comes from.
/// Compiling the script gives the same schema.
pub fn ddl_script(schema: &Schema) -> String {
    let mut s = format!("-- {} DDL written by `dbgen inspect`: {} statements.\n", schema.dialect.product(), schema.statements.len());
    let mut current_source = None;
    for st in &schema.statements {
        // `at` is `source:line:column`, and a source name may itself contain colons.
        let source = st.at.rsplitn(3, ':').nth(2).unwrap_or(&st.at);
        if current_source != Some(source) {
            let _ = write!(s, "\n-- {source}\n");
            current_source = Some(source);
        }
        let _ = write!(s, "\n{};\n", st.sql.trim_end());
    }
    s
}

/// Writes `text` to `path`, creating missing directories and replacing an existing file.
fn write_file(path: &Path, text: &str) -> Result<()> {
    let cannot = |e: std::io::Error| Error::Config(format!("cannot write {}: {e}", path.display()));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(cannot)?;
    }
    std::fs::write(path, text).map_err(cannot)
}

/// `dbgen init`: a starting configuration for the schema, every table listed with
/// `rows` rows (tables the DDL seeds, or that only views write, as `existing`). The
/// password of the database URL is replaced by `${DBGEN_PASSWORD}`.
pub fn init(config: &Config, rows: u64) -> Result<String> {
    let schema = load_schema(config)?;
    let mut s = String::new();
    let source = if config.schema.is_empty() { "the database" } else { "the DDL files" };
    let _ = writeln!(s, "# Starting configuration written by `dbgen init` from {source}.");
    let _ = writeln!(s, "# Adjust the row counts, then run `dbgen validate -c <this file>`.");
    match &config.database {
        Some(url) => {
            let (url, masked) = mask_password(url);
            if masked {
                let _ = writeln!(s, "# The password is read from the environment: export DBGEN_PASSWORD=...");
            }
            let _ = writeln!(s, "database: {}", yaml_quote(&url));
        }
        None => {
            let _ = writeln!(s, "dialect: {}", config.dialect.name());
        }
    }
    if !config.schema.is_empty() {
        let paths: Vec<String> = config.schema.iter().map(|p| yaml_quote(&p.display().to_string())).collect();
        let _ = writeln!(s, "schema: [{}]", paths.join(", "));
    }
    let g = &config.generation;
    let _ = writeln!(s, "\ngeneration:\n  seed: {}\n  history_days: {}", g.seed, g.history_days);
    let _ = writeln!(
        s,
        "  end_time: \"{}\"",
        g.end_time.clone().unwrap_or_else(|| crate::timeutil::Ymd(crate::timeutil::day_of(crate::timeutil::today_utc_midnight())).to_string())
    );
    let _ = writeln!(s, "\ntables:");
    for t in &schema.tables {
        let (value, note) = match () {
            _ if !t.storage => ("skip".to_string(), " # stores no rows".to_string()),
            _ if t.seeded => ("existing".to_string(), " # seeded by the DDL".to_string()),
            _ => match &t.derived_by {
                Some(view) => ("existing".to_string(), format!(" # written by {view}")),
                None => (rows.to_string(), String::new()),
            },
        };
        let _ = writeln!(s, "  {}: {value}{note}", yaml_key(&t.name));
    }
    Ok(s)
}

/// The URL with its password replaced by `${DBGEN_PASSWORD}`, and whether it had one.
fn mask_password(url: &str) -> (String, bool) {
    let Some((scheme, rest)) = url.split_once("://") else { return (url.to_string(), false) };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else { return (url.to_string(), false) };
    match rest[..at].split_once(':') {
        Some((user, password)) if !password.is_empty() && !password.starts_with("${") => {
            (format!("{scheme}://{user}:${{DBGEN_PASSWORD}}{}", &rest[at..]), true)
        }
        _ => (url.to_string(), false),
    }
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_key(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { s.to_string() } else { yaml_quote(s) }
}

/// The compiled schema as text, for `dbgen inspect`.
pub fn render_schema(schema: &Schema) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Schema ({}): {} tables from {} statements", schema.dialect.name(), schema.tables.len(), schema.statements.len());
    if !schema.ignored.is_empty() {
        let ignored: Vec<String> = schema.ignored.iter().map(|(k, n)| format!("{k} x{n}")).collect();
        let _ = writeln!(s, "Statements without effect on the structure: {}", ignored.join(", "));
    }
    for t in &schema.tables {
        let mut flags = Vec::new();
        if let Some(e) = &t.engine {
            flags.push(e.clone());
        }
        if t.collapsing {
            flags.push("collapsing".into());
        }
        if t.seeded {
            flags.push("seeded".into());
        }
        if let Some(v) = &t.derived_by {
            flags.push(format!("written by {v}"));
        }
        let _ = writeln!(s, "\n{} [{}]  ({})", t.name, flags.join(", "), t.defined_at);
        for c in &t.columns {
            let mut extra = String::new();
            if let Some(d) = &c.default {
                let _ = write!(extra, " {:?}", d.kind);
                if let Some(sql) = &d.sql {
                    let _ = write!(extra, " {}", sql.split_whitespace().collect::<Vec<_>>().join(" "));
                }
            }
            if let Some(f) = c.fixed {
                let _ = write!(extra, " (engine requires {f})");
            }
            let _ = writeln!(s, "  {:<34} {:<36}{extra}", c.name, c.ty.to_string());
        }
        for k in &t.keys {
            let parts: Vec<&str> = k
                .parts
                .iter()
                .map(|p| match p {
                    KeyPart::Column(c) | KeyPart::Expr(c) => c.as_str(),
                })
                .collect();
            let _ = writeln!(s, "  KEY {:?} ({})", k.kind, parts.join(", "));
        }
        for fk in &t.foreign_keys {
            let _ = writeln!(s, "  FOREIGN KEY ({}) -> {}({})", fk.columns.join(", "), fk.ref_table, fk.ref_columns.join(", "));
        }
        for ch in &t.checks {
            let _ = writeln!(s, "  CHECK {}: {}", ch.name, ch.sql.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        if let Some(p) = &t.partition_by {
            let _ = writeln!(s, "  PARTITION BY {}", p.sql);
        }
    }
    s
}

/// Compares the compiled schema with the live database: generated and read tables must
/// exist with every DDL column. Column types must match exactly only when the backend's
/// bulk format depends on them.
fn check_live(db: &Database, schema: &Schema, plan: &Plan) -> Result<()> {
    let live = db.live_columns().map_err(|e| e.into_error("reading the catalog"))?;
    let mut problems = Vec::new();
    let mut missing_tables = false;
    for (ti, tp) in plan.tables.iter().enumerate() {
        let t = &schema.tables[ti];
        let generated = matches!(tp.mode, plan::Mode::Generate(_));
        if !generated && plan.world.existing[ti].is_none() {
            continue;
        }
        let Some(cols) = live.get(&t.name) else {
            problems.push(format!("table {} does not exist in the database", t.name));
            missing_tables = true;
            continue;
        };
        if !generated {
            continue;
        }
        for c in &t.columns {
            match cols.iter().find(|(n, _)| *n == c.name) {
                None => problems.push(format!("column {}.{} is in the DDL but not in the database", t.name, c.name)),
                Some((_, ty)) if db.strict_types() && db.parse_type(ty).as_ref() != Ok(&c.ty) => {
                    problems.push(format!("column {}.{} is {} in the DDL but {ty} in the database", t.name, c.name, c.ty));
                }
                Some(_) => {}
            }
        }
        for (name, ty) in cols {
            if t.column(name).is_none() {
                tracing::warn!("column {}.{name} ({ty}) is in the database but not in the DDL: it gets its default", t.name);
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let mut message = format!("the database does not match the DDL input:\n{}", bullets(&problems));
    if missing_tables {
        // An empty database gets the schema from `generate` (see `lacks_schema`), so this
        // one holds part of it, and running every statement again would fail.
        message.push_str(
            "\nthe database holds only part of the schema: `dbgen generate` creates the schema only in a database \
             that has none of its tables; bring this one up to date with the DDL first, or empty it",
        );
    }
    Err(Error::Validation(message))
}

fn bullets(items: &[String]) -> String {
    items.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n")
}

/// The server version. The first call to reach the server, so its error says whether the
/// server or the database is missing.
fn server_version(db: &Database) -> Result<String> {
    db.version().map_err(|e| e.into_error(&format!("connecting to {}", db.describe())))
}

/// Everything a command needs once the configuration, the DDL and the database agree.
struct Prepared {
    config: Config,
    schema: Schema,
    db: Database,
    plan: Plan,
    server_version: String,
    /// The database has none of the schema's tables, and they were not created (the
    /// command writes nothing): the plan was built without reading the database, and not
    /// compared with it.
    schema_pending: bool,
}

/// Connects, compiles the schema and builds the plan. With `create_schema`, a database
/// that has none of the tables of the DDL files gets them first: every statement runs, in
/// order, as `dbgen apply` would run them.
fn prepare(config: Config, create_schema: bool) -> Result<Prepared> {
    let db = Database::connect(&config)?;
    tracing::info!(target = %db.describe(), "connecting");
    let server_version = server_version(&db)?;
    let schema = load_schema_with(&config, &db)?;
    let mut schema_pending = lacks_schema(&config, &db, &schema)?;
    if schema_pending && create_schema {
        tracing::info!(
            tables = schema.tables.len(),
            statements = schema.statements.len(),
            "the database has none of the schema's tables: running the DDL"
        );
        let started = Instant::now();
        run_ddl(&db, &schema, "creating the schema, ")?;
        tracing::info!(seconds = %format!("{:.1}", started.elapsed().as_secs_f64()), "schema created");
        schema_pending = false;
    }
    // Until the DDL runs, the tables the plan reads rather than generates (seeded by the
    // DDL, for one) do not exist: the plan is built as without a database.
    let rows: Option<&dyn plan::RowSource> = if schema_pending { None } else { Some(&db) };
    let plan = plan::build(&config, &schema, rows)?;
    for w in &plan.warnings {
        tracing::warn!("{w}");
    }
    if !schema_pending {
        check_live(&db, &schema, &plan)?;
    }
    Ok(Prepared { config, schema, db, plan, server_version, schema_pending })
}

/// Whether the schema comes from DDL files and the database has none of its tables: an
/// empty database, where `generate` runs the DDL before loading.
fn lacks_schema(config: &Config, db: &Database, schema: &Schema) -> Result<bool> {
    if config.schema.is_empty() || schema.tables.is_empty() {
        return Ok(false);
    }
    let live = db.live_columns().map_err(|e| e.into_error("reading the catalog"))?;
    Ok(!schema.tables.iter().any(|t| live.contains_key(&t.name)))
}

/// What `validate` and `generate --dry-run` say about a database that `generate` would
/// create the schema in.
fn pending_schema_note(schema: &Schema) -> String {
    format!(
        "The database has none of the schema's tables: `dbgen generate` creates them first, running the {} statements \
         of the DDL. Until then, the rows of the tables that are not generated (such as tables the DDL seeds) cannot \
         be read, so the plan below leaves references to them out.",
        schema.statements.len()
    )
}

/// Runs every statement of the DDL against the database, in order, as written. Errors
/// name the statement after `context`.
fn run_ddl(db: &Database, schema: &Schema, context: &str) -> Result<()> {
    for (k, st) in schema.statements.iter().enumerate() {
        db.execute(&st.sql).map_err(|e| e.into_error(&format!("{context}statement {} at {}", k + 1, st.at)))?;
    }
    Ok(())
}

/// Generates sample rows of every table, so CHECKs that cannot be satisfied are found
/// before anything is written. Returns the report as text.
fn check_samples(plan: &Plan) -> Result<String> {
    let mut s = String::from("\nSample rows (CHECK constraints solved before writing):\n");
    let mut problems = Vec::new();
    for r in plan::sample(plan, 500) {
        match &r.error {
            Some(e) => problems.push(e.clone()),
            None => {
                let _ = writeln!(s, "  {:<36} {} rows sampled, {} needed repair (max {} steps)", r.table, r.sampled, r.repaired, r.max_steps);
            }
        }
    }
    if problems.is_empty() {
        Ok(s)
    } else {
        Err(Error::Validation(format!(
            "rows that satisfy every CHECK could not be generated:\n{}\nconstrain the columns involved under `columns:` (values, range) or raise generation.max_repair_steps",
            bullets(&problems)
        )))
    }
}

/// `dbgen validate`: everything short of writing.
pub fn validate(config: Config) -> Result<()> {
    let p = prepare(config, false)?;
    println!("Database: {} ({} {})", p.db.describe(), p.db.dialect().product(), p.server_version);
    if p.schema_pending {
        println!("{}\n", pending_schema_note(&p.schema));
    }
    println!("{}", p.plan.render(&p.schema));
    println!("{}", check_samples(&p.plan)?);
    println!("Total rows to generate: {}", crate::report::thousands(p.plan.total_rows()));
    Ok(())
}

/// `dbgen apply` with the configuration file `config_path`.
pub fn apply(config_path: &Path, create_database: bool) -> Result<()> {
    apply_config(&Config::load(config_path)?, create_database)
}

/// `dbgen apply`: runs every statement of the DDL files against the database, in order.
pub fn apply_config(config: &Config, create_database: bool) -> Result<()> {
    if config.schema.is_empty() {
        return Err(Error::Config("apply runs DDL files, and none are given: set `schema` (--schema)".into()));
    }
    let schema = schema_from_files(config)?;
    let db = Database::connect(config)?;
    if create_database {
        db.create_database().map_err(|e| e.into_error("creating the database"))?;
    }
    server_version(&db)?;
    let started = Instant::now();
    run_ddl(&db, &schema, "")?;
    println!("Applied {} statements to {} in {:.1}s", schema.statements.len(), db.describe(), started.elapsed().as_secs_f64());
    Ok(())
}

/// Options of `dbgen generate`.
pub struct GenerateOptions {
    /// The configuration file, read by [`generate`]; [`generate_config`] takes the
    /// configuration already resolved and ignores it.
    pub config_path: PathBuf,
    /// Empty every generated table first.
    pub reset: bool,
    /// Validate and print the plan, then stop.
    pub dry_run: bool,
    /// Write the summary as JSON to this file.
    pub report: Option<PathBuf>,
    /// Progress bars on a terminal (log lines otherwise).
    pub progress_bars: bool,
    /// Run the verification checks after a successful load.
    pub verify: bool,
}

/// The result of `dbgen generate`. `summary` is absent after a dry run; `error` is set when
/// the load or the verification failed after the load started.
pub struct Outcome {
    pub summary: Option<Summary>,
    pub verification: Option<String>,
    pub error: Option<Error>,
}

/// `dbgen generate` with the configuration file `opts.config_path`.
pub fn generate(opts: GenerateOptions) -> Result<Outcome> {
    let config = Config::load(&opts.config_path)?;
    generate_config(config, &opts)
}

/// `dbgen generate`: loads the configured rows, resuming an interrupted run. A database
/// that has none of the tables of the DDL files gets them first (see [`prepare`]); a dry
/// run only says so.
pub fn generate_config(config: Config, opts: &GenerateOptions) -> Result<Outcome> {
    let started = Instant::now();
    let p = prepare(config, !opts.dry_run)?;
    let samples = check_samples(&p.plan)?;
    if opts.dry_run {
        if p.schema_pending {
            println!("{}\n", pending_schema_note(&p.schema));
        }
        println!("{}", p.plan.render(&p.schema));
        println!("{samples}");
        return Ok(Outcome { summary: None, verification: None, error: None });
    }
    let fingerprint = p.plan.fingerprint(&p.schema);
    let state_path = state::path_for(&p.config, &p.db.state_key());
    let mut st = resume_state(&p, &fingerprint, &state_path, opts.reset)?;
    st.completed = false;
    st.save(&state_path)?;

    let g = &p.config.generation;
    engine::install_interrupt_handler();
    let stats = Arc::new(Stats::new(&p.plan, g.batch_size));
    let progress = Progress::start(stats.clone(), opts.progress_bars, Duration::from_secs(g.progress_interval_secs.max(1)));
    let state = Arc::new(Mutex::new(st));
    let load_started = Instant::now();
    let load = Load {
        config: &p.config,
        schema: &p.schema,
        plan: &p.plan,
        db: &p.db,
        stats: stats.clone(),
        state: state.clone(),
        state_path: state_path.clone(),
    };
    let result = load.run();
    let load_seconds = load_started.elapsed().as_secs_f64();
    progress.stop();

    let mut error = result.err();
    if error.is_none() {
        let tables: Vec<&schema::Table> = p.plan.generated().map(|(ti, _)| &p.schema.tables[ti]).collect();
        if let Err(e) = p.db.finish_load(&tables) {
            tracing::warn!("after the load: {e}");
        }
    }
    let interrupted = matches!(error, Some(Error::Interrupted));
    let finals: Vec<Option<u64>> = stats.tables.iter().map(|t| p.db.count(&t.name).ok()).collect();
    let tables = Summary::tables_from(&stats, &finals, interrupted);
    if error.is_none() && !tables.iter().all(|t| t.final_rows == Some(t.target_rows)) {
        error = Some(Error::Db("final row counts differ from the targets (see Final Rows)".into()));
    }
    if error.is_none() {
        let mut s = state.lock().unwrap();
        s.completed = true;
        s.save(&state_path)?;
    }
    let mut verification = None;
    if opts.verify && error.is_none() {
        let results = verify::run(&p.db, &p.schema, &p.plan);
        verification = Some(verify::render(&results));
        if !verify::all_passed(&results) {
            error = Some(Error::Verification("some verification checks failed".into()));
        }
    }
    let summary = Summary {
        database: p.db.describe(),
        server_version: p.server_version.clone(),
        fingerprint,
        seed: g.seed,
        window_start: Rfc3339(p.plan.window.0).to_string(),
        window_end: Rfc3339(p.plan.window.1).to_string(),
        batch_size: g.batch_size,
        workers: g.workers,
        tables,
        load_seconds,
        total_seconds: started.elapsed().as_secs_f64(),
        status: match &error {
            None => OverallStatus::Success,
            Some(Error::Interrupted) => OverallStatus::Interrupted,
            Some(_) => OverallStatus::Failed,
        },
        error: error.as_ref().map(ToString::to_string),
    };
    if let Some(path) = &opts.report {
        write_json(path, &summary)?;
    }
    Ok(Outcome { summary: Some(summary), verification, error })
}

/// The state to load with: a new one after `--fresh` or on an empty database, or the state
/// of the interrupted run to resume. Generated tables whose rows do not match the state
/// are emptied, with the tables that reference them when the database checks foreign keys.
fn resume_state(p: &Prepared, fingerprint: &str, state_path: &Path, reset: bool) -> Result<State> {
    let g = &p.config.generation;
    let fresh = State {
        format: state::FORMAT,
        dbgen_version: env!("CARGO_PKG_VERSION").into(),
        fingerprint: fingerprint.to_string(),
        database: p.db.describe(),
        seed: g.seed,
        batch_size: g.batch_size,
        window: (Rfc3339(p.plan.window.0).to_string(), Rfc3339(p.plan.window.1).to_string()),
        targets: p.plan.generated().map(|(_, t)| (t.name.clone(), t.rows)).collect(),
        tables: Default::default(),
        completed: false,
        started_at: chrono::Utc::now().to_rfc3339(),
        updated_at: String::new(),
    };
    let truncate = |names: &[&str]| p.db.truncate(names).map_err(|e| e.into_error(&format!("emptying {}", names.join(", "))));
    let count = |t: &str| p.db.count(t).map_err(|e| e.into_error(&format!("counting {t}")));
    let generated: Vec<(usize, &str)> = p.plan.generated().map(|(ti, t)| (ti, t.name.as_str())).collect();

    if reset {
        truncate(&generated.iter().map(|(_, n)| *n).collect::<Vec<_>>())?;
        tracing::info!(tables = generated.len(), "fresh start: generated tables emptied");
        return Ok(fresh);
    }
    let Some(mut old) = State::load(state_path)? else {
        let mut busy = Vec::new();
        for (_, name) in &generated {
            let n = count(name)?;
            if n > 0 {
                busy.push(format!("{name} ({n} rows)"));
            }
        }
        if !busy.is_empty() {
            return Err(Error::Validation(format!(
                "tables to generate already hold rows dbgen did not write: {}\nuse --fresh to empty them first",
                busy.join(", ")
            )));
        }
        return Ok(fresh);
    };
    let diffs = old.differences(&fresh);
    if !diffs.is_empty() {
        return Err(Error::Validation(format!(
            "{} holds a run with different settings; restore them, or start over with --fresh:\n{}",
            state_path.display(),
            bullets(&diffs)
        )));
    }
    let mut reload: Vec<usize> = Vec::new();
    for &(ti, name) in &generated {
        let (live, recorded) = (count(name)?, old.rows(name));
        if live != recorded && !reload.contains(&ti) {
            tracing::warn!(table = %name, live, recorded, "row count differs from the resume state: reloading the table");
            reload.push(ti);
            if p.db.dialect().enforces_foreign_keys() {
                for d in p.plan.dependents(ti) {
                    if !reload.contains(&d) {
                        reload.push(d);
                    }
                }
            }
        }
    }
    if !reload.is_empty() {
        let names: Vec<&str> = reload.iter().map(|&ti| p.schema.tables[ti].name.as_str()).collect();
        truncate(&names)?;
        names.iter().for_each(|n| old.forget(n));
    }
    if old.completed && generated.iter().all(|&(ti, name)| old.rows(name) == p.plan.world.table(ti).rows) {
        tracing::info!("the previous run with these settings completed; nothing to do");
    }
    Ok(old)
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value).expect("serializable"))
        .map_err(|e| Error::Config(format!("cannot write report {}: {e}", path.display())))
}

/// `dbgen verify`. Returns whether every check passed.
pub fn verify_only(config: &Config, report: Option<&Path>) -> Result<bool> {
    let db = Database::connect(config)?;
    server_version(&db)?;
    let schema = load_schema_with(config, &db)?;
    let plan = plan::build(config, &schema, None)?;
    let results = verify::run(&db, &schema, &plan);
    println!("{}", verify::render(&results));
    if let Some(path) = report {
        write_json(path, &results)?;
    }
    Ok(verify::all_passed(&results))
}

#[cfg(all(test, feature = "clickhouse"))]
mod tests {
    use super::*;
    use crate::dialect::Dialect;

    #[test]
    fn ddl_script_compiles_back_to_the_same_schema() {
        let sources = [
            Source::new("m/001.sql", "CREATE TABLE t (id UInt32) ENGINE = MergeTree ORDER BY id; -- note\nINSERT INTO t VALUES (1)".into()),
            Source::new("m/002.sql", "ALTER TABLE t ADD COLUMN x Nullable(String);\nOPTIMIZE TABLE t FINAL;".into()),
        ];
        let schema = schema::compile(Dialect::Clickhouse, &sources).unwrap();
        let script = ddl_script(&schema);
        assert!(script.contains("\n-- m/001.sql\n") && script.contains("\n-- m/002.sql\n"), "{script}");
        assert_eq!(script.matches(";\n").count(), 4, "{script}");

        let again = schema::compile(Dialect::Clickhouse, &[Source::new("dump.sql", script)]).unwrap();
        let shape = |s: &Schema| s.tables.iter().map(|t| (t.name.clone(), t.seeded, t.columns.iter().map(|c| (c.name.clone(), c.ty.to_string())).collect::<Vec<_>>())).collect::<Vec<_>>();
        assert_eq!(shape(&again), shape(&schema));
        assert_eq!(again.statements.len(), schema.statements.len());
    }
}
