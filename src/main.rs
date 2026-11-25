use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use dbgen::config::{Config, Overrides};
use dbgen::dialect::Dialect;
use dbgen::error::{Error, Result};
use dbgen::progress::LogWriter;
use dbgen::run::{self, GenerateOptions};

/// Database-agnostic synthetic data generator: reads a schema (from DDL files or from the
/// database itself), generates rows that satisfy it (keys, references, CHECK constraints)
/// and bulk-loads them into PostgreSQL, MySQL, SQLite or ClickHouse.
///
/// Settings come from a configuration file (YAML or JSON, `dbgen.yaml` by default) and from
/// the flags below, which take precedence. Without a file, the flags are enough:
///
///   dbgen generate -d postgres://user:pass@localhost/shop --default-rows 1000 --verify
///
/// Exit codes: 0 success, 1 load or verification failure, 2 invalid configuration or
/// schema, 3 connection failure, 130 interrupted (resumable).
#[derive(Parser)]
#[command(version, about, long_about, max_term_width = 100)]
struct Cli {
    /// Log level, or a tracing filter such as `dbgen=debug,info`.
    #[arg(long, global = true, default_value = "info", env = "DBGEN_LOG", help_heading = "Logging")]
    log_level: String,

    /// Log line format.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Pretty, help_heading = "Logging")]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

/// Where the settings, the schema and the data go.
#[derive(Args)]
#[command(next_help_heading = "Target")]
struct Target {
    /// Configuration file, YAML or JSON. Default: dbgen.yaml in the current directory, if any.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Database URL: postgres://, mysql://, sqlite:, clickhouse:// or http(s)://. Overrides `database`.
    #[arg(short, long, value_name = "URL", env = "DBGEN_DATABASE", hide_env_values = true)]
    database: Option<String>,

    /// SQL dialect of the schema. Default: from the database URL.
    #[arg(long, value_name = "NAME")]
    dialect: Option<Dialect>,

    /// DDL file or directory, repeatable; overrides `schema`. Without any, the schema is read
    /// from the database.
    #[arg(short, long = "schema", value_name = "PATH")]
    schema: Vec<PathBuf>,
}

/// What to generate.
#[derive(Args)]
#[command(next_help_heading = "Data")]
struct Data {
    /// Rows of a table: TABLE=N, TABLE=existing or TABLE=skip. Repeatable or comma-separated.
    #[arg(short, long = "rows", value_name = "TABLE=N", value_delimiter = ',')]
    rows: Vec<String>,

    /// Rows for every table that `tables` and --rows do not list.
    #[arg(long, value_name = "N")]
    default_rows: Option<u64>,

    /// Seed: the same seed, schema and settings give the same data.
    #[arg(long, value_name = "N")]
    seed: Option<u64>,

    /// Length of the time window, in days.
    #[arg(long, value_name = "DAYS")]
    history_days: Option<u32>,

    /// End of the time window: YYYY-MM-DD or a timestamp. Default: today 00:00 UTC.
    #[arg(long, value_name = "WHEN")]
    end_time: Option<String>,

    /// Share of NULLs in nullable columns, 0 to 1.
    #[arg(long, value_name = "RATE")]
    null_rate: Option<f64>,

    /// Refuse to run when a table of the schema has no target, instead of leaving it alone.
    #[arg(long)]
    strict_tables: bool,
}

/// How to load.
#[derive(Args)]
#[command(next_help_heading = "Load")]
struct Load {
    /// Rows per batch; each batch is inserted atomically.
    #[arg(long, value_name = "N")]
    batch_size: Option<u64>,

    /// Concurrent writing connections.
    #[arg(short, long, value_name = "N")]
    workers: Option<usize>,

    /// Tables of one dependency level loaded at the same time.
    #[arg(long, value_name = "N")]
    table_concurrency: Option<usize>,

    /// Attempts per batch on transient failures.
    #[arg(long, value_name = "N")]
    max_retries: Option<u32>,

    /// Where to keep the resume state. Default: .dbgen/<target>.json next to the configuration.
    #[arg(long, value_name = "FILE")]
    state_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the schema, and the generation plan when there are row targets. Needs the
    /// database only when the schema is read from it.
    Inspect {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        data: Data,
        /// Print the schema as JSON.
        #[arg(long)]
        json: bool,
        /// DDL files or directories (same as --schema).
        files: Vec<PathBuf>,
    },
    /// Write a starting configuration listing every table of the schema.
    Init {
        #[command(flatten)]
        target: Target,
        /// Rows to propose for each table.
        #[arg(long, value_name = "N", default_value_t = 1000)]
        rows: u64,
        /// Write to this file instead of standard output.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Overwrite the output file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Check the settings against the schema and the live database, generate sample rows,
    /// and print the plan. Writes nothing.
    Validate {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        data: Data,
    },
    /// Run every statement of the DDL files against the database, in order, as written.
    Apply {
        #[command(flatten)]
        target: Target,
        /// Create the database first if it does not exist.
        #[arg(long)]
        create_database: bool,
    },
    /// Generate and load the rows (resumes an interrupted run automatically).
    Generate {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        data: Data,
        #[command(flatten)]
        load: Load,
        /// Empty every generated table first.
        #[arg(long, help_heading = "Run")]
        reset: bool,
        /// Validate and print the plan, then stop.
        #[arg(long, help_heading = "Run")]
        dry_run: bool,
        /// Run the verification checks after a successful load.
        #[arg(long, help_heading = "Run")]
        verify: bool,
        /// Write the summary as JSON to this file.
        #[arg(long, value_name = "FILE", help_heading = "Run")]
        report: Option<PathBuf>,
        /// Periodic log lines instead of progress bars.
        #[arg(long, help_heading = "Run")]
        no_progress: bool,
    },
    /// Check row counts, unique keys, references and CHECK constraints in the database.
    /// Writes nothing.
    Verify {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        data: Data,
        /// Write the results as JSON to this file.
        #[arg(long, value_name = "FILE")]
        report: Option<PathBuf>,
    },
}

/// The configuration of a command: the file (given, or `dbgen.yaml` if present) under the
/// flags.
fn resolve(target: Target, data: Option<Data>, load: Option<Load>) -> Result<Config> {
    let mut o = Overrides {
        database: target.database,
        dialect: target.dialect.map(|d| d.name().to_string()),
        schema: target.schema,
        ..Overrides::default()
    };
    if let Some(d) = data {
        o.rows = d.rows;
        o.set("default_rows", d.default_rows);
        o.set("seed", d.seed);
        o.set("history_days", d.history_days.map(u64::from));
        o.set("end_time", d.end_time);
        o.set("null_rate", d.null_rate);
        if d.strict_tables {
            o.set("missing_tables", Some("error"));
        }
    }
    if let Some(l) = load {
        o.set("batch_size", l.batch_size);
        o.set("workers", l.workers.map(|n| n as u64));
        o.set("table_concurrency", l.table_concurrency.map(|n| n as u64));
        o.set("max_retries", l.max_retries.map(u64::from));
        o.set_path("state_file", l.state_file.as_deref())?;
    }
    let default = Path::new("dbgen.yaml");
    let file = target.config.or_else(|| default.exists().then(|| default.to_path_buf()));
    Config::resolve(file.as_deref(), &o)
}

fn run(command: Command) -> Result<i32> {
    match command {
        Command::Inspect { mut target, data, json, files } => {
            target.schema.extend(files);
            run::inspect(&resolve(target, Some(data), None)?, json)?;
        }
        Command::Init { target, rows, output, force } => {
            let text = run::init(&resolve(target, None, None)?, rows)?;
            match output {
                None => print!("{text}"),
                Some(path) if path.exists() && !force => {
                    return Err(Error::Config(format!("{} exists; use --force to overwrite it", path.display())))
                }
                Some(path) => {
                    std::fs::write(&path, text).map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))?;
                    eprintln!("Wrote {}", path.display());
                }
            }
        }
        Command::Validate { target, data } => run::validate(resolve(target, Some(data), None)?)?,
        Command::Apply { target, create_database } => run::apply_config(&resolve(target, None, None)?, create_database)?,
        Command::Generate { target, data, load, reset, dry_run, verify, report, no_progress } => {
            let config = resolve(target, Some(data), Some(load))?;
            let opts = GenerateOptions { config_path: config.path.clone(), reset, dry_run, report, progress_bars: !no_progress, verify };
            let outcome = run::generate_config(config, &opts)?;
            if let Some(summary) = &outcome.summary {
                println!("{}", summary.render());
            }
            if let Some(v) = &outcome.verification {
                println!("{v}");
            }
            if let Some(e) = outcome.error {
                return Ok(fail(&e));
            }
        }
        Command::Verify { target, data, report } => {
            if !run::verify_only(&resolve(target, Some(data), None)?, report.as_deref())? {
                return Ok(1);
            }
        }
    }
    Ok(0)
}

fn init_logging(level: &str, format: LogFormat) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_writer(|| LogWriter).with_target(false);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().flatten_event(true).init(),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log_level, cli.log_format);
    let code = run(cli.command).unwrap_or_else(|e| fail(&e));
    ExitCode::from(code as u8)
}

fn fail(e: &Error) -> i32 {
    tracing::error!("{e}");
    e.exit_code()
}
