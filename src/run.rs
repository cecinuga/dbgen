//! The commands: inspect, validate, apply, generate, verify.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::Database;
use crate::dialect::Dialect;
use crate::engine::{self, Load, Stats};
use crate::error::{Error, Result};
use crate::plan::{self, Plan};
use crate::progress::Progress;
use crate::report::{OverallStatus, Summary};
use crate::schema::{self, KeyPart, Schema};
use crate::state::{self, State};
use crate::timeutil::Rfc3339;
use crate::verify;

pub fn load_schema(config: &Config) -> Result<Schema> {
    if config.schema.is_empty() {
        return Err(Error::Config(format!("{}: 'schema' lists no DDL files or directories", config.path.display())));
    }
    let sources = schema::read_sources(&config.schema, &config.base_dir)?;
    let schema = schema::compile(config.dialect, &sources)?;
    for w in &schema.warnings {
        tracing::warn!("{w}");
    }
    Ok(schema)
}

/// `dbgen inspect`: the compiled schema, and the plan when a configuration is given.
pub fn inspect(config_path: Option<&Path>, files: &[PathBuf], dialect: Option<Dialect>, json: bool) -> Result<()> {
    let (schema, config) = match config_path {
        Some(p) if files.is_empty() => {
            let config = Config::load(p)?;
            (load_schema(&config)?, Some(config))
        }
        _ => {
            if files.is_empty() {
                return Err(Error::Config("give DDL files or directories, or --config".into()));
            }
            let sources = schema::read_sources(files, Path::new("."))?;
            (schema::compile(dialect.unwrap_or(Dialect::Clickhouse), &sources)?, None)
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&schema).expect("serializable"));
        return Ok(());
    }
    println!("{}", render_schema(&schema));
    if let Some(config) = config {
        let plan = plan::build(&config, &schema, None)?;
        println!("{}", plan.render(&schema));
        println!("(Without a database: rows of tables that are referenced but not generated are not read. `dbgen validate` reads them.)");
    }
    Ok(())
}

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
            let parts: Vec<&str> = k.parts.iter().map(|p| match p {
                KeyPart::Column(c) | KeyPart::Expr(c) => c.as_str(),
            }).collect();
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

/// Compares the compiled schema with the live database.
fn check_live(db: &Database, schema: &Schema, plan: &Plan) -> Result<()> {
    let live = db.live_columns()?;
    let mut problems = Vec::new();
    for (ti, tp) in plan.tables.iter().enumerate() {
        let t = &schema.tables[ti];
        let generated = matches!(tp.mode, plan::Mode::Generate(_));
        let read = plan.world.existing[ti].is_some();
        if !generated && !read {
            continue;
        }
        let Some(cols) = live.get(&t.name) else {
            problems.push(format!("table {} does not exist in the database (`dbgen apply` creates the schema)", t.name));
            continue;
        };
        if !generated {
            continue;
        }
        for c in &t.columns {
            match cols.iter().find(|(n, _)| *n == c.name) {
                None => problems.push(format!("column {}.{} is in the DDL but not in the database", t.name, c.name)),
                Some((_, ty)) => match db.parse_type(ty) {
                    Ok(live_ty) if live_ty == c.ty => {}
                    Ok(_) | Err(_) => problems.push(format!("column {}.{} is {} in the DDL but {ty} in the database", t.name, c.name, c.ty)),
                },
            }
        }
        for (name, ty) in cols {
            if t.column(name).is_none() {
                tracing::warn!("column {}.{name} ({ty}) is in the database but not in the DDL: it gets its default", t.name);
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "the database does not match the DDL input:\n{}",
            problems.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n")
        )))
    }
}

/// The server version; the first call, so a missing database gets a useful hint.
fn server_version(db: &Database) -> Result<String> {
    db.version().map_err(|e| match e {
        Error::Db(m) if m.contains("UNKNOWN_DATABASE") => Error::Db(format!("{m}\n`dbgen apply --create-database` creates it and applies the DDL")),
        other => other,
    })
}

struct Prepared {
    config: Config,
    schema: Schema,
    db: Database,
    plan: Plan,
    server_version: String,
}

fn prepare(config_path: &Path) -> Result<Prepared> {
    let config = Config::load(config_path)?;
    let schema = load_schema(&config)?;
    let db = Database::connect(&config)?;
    tracing::info!(target = %db.describe(), "connecting");
    let server_version = server_version(&db)?;
    let plan = plan::build(&config, &schema, Some(&db))?;
    for w in &plan.warnings {
        tracing::warn!("{w}");
    }
    check_live(&db, &schema, &plan)?;
    Ok(Prepared { config, schema, db, plan, server_version })
}

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
            problems.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n")
        )))
    }
}

/// `dbgen validate`: everything short of writing.
pub fn validate(config_path: &Path) -> Result<()> {
    let p = prepare(config_path)?;
    println!("Database: {} (ClickHouse {})", p.db.describe(), p.server_version);
    println!("{}", p.plan.render(&p.schema));
    println!("{}", check_samples(&p.plan)?);
    println!("Total rows to generate: {}", crate::report::thousands(p.plan.total_rows()));
    Ok(())
}

/// `dbgen apply`: runs every statement of the DDL input against the database, in order.
pub fn apply(config_path: &Path, create_database: bool) -> Result<()> {
    let config = Config::load(config_path)?;
    let schema = load_schema(&config)?;
    let db = Database::connect(&config)?;
    if create_database {
        let Database::Clickhouse(c) = &db;
        let mut admin = c.clone();
        let name = admin.database.clone();
        admin.database = "default".into();
        admin.execute(&format!("CREATE DATABASE IF NOT EXISTS {}", crate::db::clickhouse::Client::quote(&name))).map_err(|e| e.into_error("creating the database"))?;
    }
    server_version(&db)?;
    let started = Instant::now();
    for (k, st) in schema.statements.iter().enumerate() {
        db.execute(&st.sql).map_err(|e| e.into_error(&format!("statement {} at {}", k + 1, st.at)))?;
    }
    println!("Applied {} statements to {} in {:.1}s", schema.statements.len(), db.describe(), started.elapsed().as_secs_f64());
    Ok(())
}

pub struct GenerateOptions {
    pub config_path: PathBuf,
    pub reset: bool,
    pub dry_run: bool,
    pub report: Option<PathBuf>,
    pub progress_bars: bool,
    pub verify: bool,
}

pub struct Outcome {
    pub summary: Option<Summary>,
    pub verification: Option<String>,
    pub error: Option<Error>,
}

pub fn generate(opts: GenerateOptions) -> Result<Outcome> {
    let started = Instant::now();
    let p = prepare(&opts.config_path)?;
    let samples = check_samples(&p.plan)?;
    if opts.dry_run {
        println!("{}", p.plan.render(&p.schema));
        println!("{samples}");
        return Ok(Outcome { summary: None, verification: None, error: None });
    }
    let g = &p.config.generation;
    let fingerprint = p.plan.fingerprint(&p.schema);
    let fresh = State {
        format: state::FORMAT,
        dbgen_version: env!("CARGO_PKG_VERSION").into(),
        fingerprint: fingerprint.clone(),
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
    let state_path = state::path_for(&p.config, &p.db.state_key());
    let count = |t: &str| p.db.count(t).map_err(|e| e.into_error(&format!("counting {t}")));
    let mut st = if opts.reset {
        for (_, tg) in p.plan.generated() {
            p.db.truncate(&tg.name).map_err(|e| e.into_error(&format!("truncating {}", tg.name)))?;
        }
        tracing::info!(tables = p.plan.order.len(), "reset: generated tables truncated");
        fresh
    } else {
        match State::load(&state_path)? {
            Some(mut old) => {
                let diffs = old.differences(&fresh);
                if !diffs.is_empty() {
                    return Err(Error::Validation(format!(
                        "{} holds a run with different settings; restore them, or start over with --reset:\n{}",
                        state_path.display(),
                        diffs.iter().map(|d| format!("  - {d}")).collect::<Vec<_>>().join("\n")
                    )));
                }
                for (_, tg) in p.plan.generated() {
                    let live = count(&tg.name)?;
                    let recorded = old.rows(&tg.name);
                    if live != recorded {
                        tracing::warn!(table = %tg.name, live, recorded, "row count differs from the resume state: reloading the table");
                        p.db.truncate(&tg.name).map_err(|e| e.into_error(&format!("truncating {}", tg.name)))?;
                        old.forget(&tg.name);
                    }
                }
                if old.completed && p.plan.generated().all(|(_, tg)| old.rows(&tg.name) == tg.rows) {
                    tracing::info!("the previous run with these settings completed; nothing to do");
                }
                old
            }
            None => {
                let mut busy = Vec::new();
                for (_, tg) in p.plan.generated() {
                    let n = count(&tg.name)?;
                    if n > 0 {
                        busy.push(format!("{} ({n} rows)", tg.name));
                    }
                }
                if !busy.is_empty() {
                    return Err(Error::Validation(format!(
                        "tables to generate already hold rows dbgen did not write: {}\nuse --reset to empty them first",
                        busy.join(", ")
                    )));
                }
                fresh
            }
        }
    };
    st.completed = false;
    st.save(&state_path)?;

    engine::install_interrupt_handler();
    let stats = Arc::new(Stats::new(&p.plan, g.batch_size));
    let progress = Progress::start(stats.clone(), opts.progress_bars, Duration::from_secs(g.progress_interval_secs.max(1)));
    let state = Arc::new(Mutex::new(st));
    let load_started = Instant::now();
    let result = Load { config: &p.config, schema: &p.schema, plan: &p.plan, db: &p.db, stats: stats.clone(), state: state.clone(), state_path: state_path.clone() }.run();
    let load_seconds = load_started.elapsed().as_secs_f64();
    progress.stop();

    let interrupted = matches!(result, Err(Error::Interrupted));
    let finals: Vec<Option<u64>> = stats.tables.iter().map(|t| p.db.count(&t.name).ok()).collect();
    let tables = Summary::tables_from(&stats, &finals, interrupted);
    let all_match = tables.iter().all(|t| t.final_rows == Some(t.target_rows));
    let mut error = result.err();
    if error.is_none() && !all_match {
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
        error: error.as_ref().map(|e| e.to_string()),
    };
    if let Some(path) = &opts.report {
        std::fs::write(path, serde_json::to_string_pretty(&summary).expect("serializable"))
            .map_err(|e| Error::Config(format!("cannot write report {}: {e}", path.display())))?;
    }
    Ok(Outcome { summary: Some(summary), verification, error })
}

/// `dbgen verify`.
pub fn verify_only(config_path: &Path, report: Option<&Path>) -> Result<bool> {
    let config = Config::load(config_path)?;
    let schema = load_schema(&config)?;
    let db = Database::connect(&config)?;
    server_version(&db)?;
    let plan = plan::build(&config, &schema, None)?;
    let results = verify::run(&db, &schema, &plan);
    println!("{}", verify::render(&results));
    if let Some(path) = report {
        std::fs::write(path, serde_json::to_string_pretty(&results).expect("serializable"))
            .map_err(|e| Error::Config(format!("cannot write report {}: {e}", path.display())))?;
    }
    Ok(verify::all_passed(&results))
}
