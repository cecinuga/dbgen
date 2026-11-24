use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use dbgen::dialect::Dialect;
use dbgen::progress::LogWriter;
use dbgen::run::{self, GenerateOptions};

/// Schema-agnostic synthetic data generator: compiles SQL DDL into an in-memory schema,
/// generates rows that satisfy it (keys, references, CHECK constraints) and bulk-loads
/// them into the database.
///
/// Exit codes: 0 success, 1 load or verification failure, 2 invalid configuration or
/// schema, 3 connection failure, 130 interrupted (resumable).
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Log level, or a tracing filter such as `dbgen=debug,info`.
    #[arg(long, global = true, default_value = "info", env = "DBGEN_LOG")]
    log_level: String,

    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Compile DDL and print the schema (and the generation plan with --config). No database.
    Inspect {
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// SQL dialect of FILES.
        #[arg(long, value_enum)]
        dialect: Option<Dialect>,
        /// Print the compiled schema as JSON.
        #[arg(long)]
        json: bool,
        /// DDL files or directories, instead of the configuration's.
        files: Vec<PathBuf>,
    },
    /// Check the configuration against the DDL and the live database, generate sample rows,
    /// and print the plan. Writes nothing.
    Validate {
        #[arg(short, long, default_value = "dbgen.yaml")]
        config: PathBuf,
    },
    /// Run every statement of the DDL input against the database, in order.
    Apply {
        #[arg(short, long, default_value = "dbgen.yaml")]
        config: PathBuf,
        /// Create the database first if it does not exist.
        #[arg(long)]
        create_database: bool,
    },
    /// Generate and load the configured rows (resumes an interrupted run automatically).
    Generate {
        #[arg(short, long, default_value = "dbgen.yaml")]
        config: PathBuf,
        /// Empty every generated table first.
        #[arg(long)]
        reset: bool,
        /// Validate and print the plan, then stop.
        #[arg(long)]
        dry_run: bool,
        /// Write the summary as JSON to this file.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Periodic log lines instead of progress bars.
        #[arg(long)]
        no_progress: bool,
        /// Run the verification checks after a successful load.
        #[arg(long)]
        verify: bool,
    },
    /// Check row counts, unique keys, references and CHECK constraints in the database.
    Verify {
        #[arg(short, long, default_value = "dbgen.yaml")]
        config: PathBuf,
        /// Write the results as JSON to this file.
        #[arg(long)]
        report: Option<PathBuf>,
    },
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
    let code = match cli.command {
        Command::Inspect { config, dialect, json, files } => {
            let config = config.or_else(|| files.is_empty().then(|| PathBuf::from("dbgen.yaml")));
            exit(run::inspect(config.as_deref(), &files, dialect, json).map(|_| true))
        }
        Command::Validate { config } => exit(run::validate(&config).map(|_| true)),
        Command::Apply { config, create_database } => exit(run::apply(&config, create_database).map(|_| true)),
        Command::Generate { config, reset, dry_run, report, no_progress, verify } => {
            let opts = GenerateOptions { config_path: config, reset, dry_run, report, progress_bars: !no_progress, verify };
            match run::generate(opts) {
                Ok(outcome) => {
                    if let Some(summary) = &outcome.summary {
                        println!("{}", summary.render());
                    }
                    if let Some(v) = &outcome.verification {
                        println!("{v}");
                    }
                    match outcome.error {
                        None => 0,
                        Some(e) => {
                            tracing::error!("{e}");
                            e.exit_code()
                        }
                    }
                }
                Err(e) => fail(&e),
            }
        }
        Command::Verify { config, report } => exit(run::verify_only(&config, report.as_deref())),
    };
    ExitCode::from(code as u8)
}

fn exit(r: dbgen::error::Result<bool>) -> i32 {
    match r {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => fail(&e),
    }
}

fn fail(e: &dbgen::error::Error) -> i32 {
    tracing::error!("{e}");
    e.exit_code()
}
