//! The load pipeline: generator threads produce encoded batches, writer threads insert
//! them, each batch atomically; committed batches are recorded for resuming.
//!
//! ```text
//!   work queue ──> generator threads ──(bounded channel)──> writer threads ──> database
//!   (table, batch)    rows + encoding         queue_depth          insert, retries
//! ```
//!
//! The work is split into phases, and a phase starts once the previous one has
//! committed. On databases that check foreign keys, each dependency level of the plan is
//! a phase, so every parent row exists before a child row references it, and a table
//! that references itself loads one batch per phase, in row order. On ClickHouse,
//! everything is one phase.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::{Batch, Database, RowRef};
use crate::error::{Error, Result};
use crate::gen::Row;
use crate::plan::Plan;
use crate::schema::{DataType, Schema};
use crate::state::State;

/// Live counters of one table.
pub struct TableStats {
    pub name: String,
    /// Index of the table in the schema.
    pub index: usize,
    pub target: u64,
    /// Rows inserted by this run.
    pub inserted: AtomicU64,
    /// Rows committed by a previous, interrupted run.
    pub resumed: AtomicU64,
    /// Encoded bytes sent.
    pub bytes: AtomicU64,
    /// Batches retried after a transient failure.
    pub retries: AtomicU64,
    /// Rows that needed the CHECK repair search.
    pub repaired: AtomicU64,
    pub batches_total: u64,
    batches_left: AtomicU64,
    started: Mutex<Option<Instant>>,
    finished: Mutex<Option<Instant>>,
}

impl TableStats {
    /// Rows in the database: inserted now or before.
    pub fn done(&self) -> u64 {
        self.inserted.load(Ordering::Relaxed) + self.resumed.load(Ordering::Relaxed)
    }

    pub fn is_started(&self) -> bool {
        self.started.lock().unwrap().is_some()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.lock().unwrap().is_some()
    }

    /// Time since the first batch was generated, until the last one committed.
    pub fn elapsed(&self) -> Option<Duration> {
        let start = (*self.started.lock().unwrap())?;
        Some(self.finished.lock().unwrap().unwrap_or_else(Instant::now) - start)
    }

    fn start(&self) {
        self.started.lock().unwrap().get_or_insert_with(Instant::now);
    }

    fn finish(&self) {
        self.start();
        *self.finished.lock().unwrap() = Some(Instant::now());
    }
}

/// Live counters of a load, shared with the progress display.
pub struct Stats {
    /// Generated tables, in load order.
    pub tables: Vec<TableStats>,
}

impl Stats {
    /// Zeroed counters for the generated tables of `plan`.
    pub fn new(plan: &Plan, batch_size: u64) -> Stats {
        let tables = plan
            .generated()
            .map(|(ti, tg)| {
                let batches = tg.rows.div_ceil(batch_size);
                TableStats {
                    name: tg.name.clone(),
                    index: ti,
                    target: tg.rows,
                    inserted: AtomicU64::new(0),
                    resumed: AtomicU64::new(0),
                    bytes: AtomicU64::new(0),
                    retries: AtomicU64::new(0),
                    repaired: AtomicU64::new(0),
                    batches_total: batches,
                    batches_left: AtomicU64::new(batches),
                    started: Mutex::new(None),
                    finished: Mutex::new(None),
                }
            })
            .collect();
        Stats { tables }
    }

    /// Rows to load in total.
    pub fn target(&self) -> u64 {
        self.tables.iter().map(|t| t.target).sum()
    }

    /// Rows in the database so far.
    pub fn done(&self) -> u64 {
        self.tables.iter().map(TableStats::done).sum()
    }
}

/// Set by the first Ctrl-C, or by a failure: stop taking new batches, let in-flight
/// ones finish.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// Installs the Ctrl-C handler: the first interrupt sets [`STOP`], the second exits.
pub fn install_interrupt_handler() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = ctrlc::set_handler(|| {
        if STOP.swap(true, Ordering::SeqCst) {
            eprintln!("\nsecond interrupt: exiting now; uncommitted batches are discarded by the server");
            std::process::exit(130);
        }
        eprintln!("\ninterrupt: finishing in-flight batches (Ctrl-C again to exit at once)");
    });
}

/// One load: everything it reads, and where it records progress.
pub struct Load<'a> {
    pub config: &'a Config,
    pub schema: &'a Schema,
    pub plan: &'a Plan,
    pub db: &'a Database,
    pub stats: Arc<Stats>,
    pub state: Arc<Mutex<State>>,
    pub state_path: PathBuf,
}

/// One batch to load: batch `batch` of `stats.tables[table]`.
#[derive(Clone, Copy)]
struct Unit {
    table: usize,
    batch: u64,
}

struct Encoded {
    unit: Unit,
    rows: u64,
    batch: Batch,
}

impl Load<'_> {
    /// Runs the load. Returns `Err(Interrupted)` after a Ctrl-C, having recorded every
    /// committed batch.
    pub fn run(&self) -> Result<()> {
        for phase in self.phases() {
            self.run_phase(&phase)?;
        }
        Ok(())
    }

    /// The batches still to load, grouped in phases (see the module documentation).
    fn phases(&self) -> Vec<Vec<Unit>> {
        let state = self.state.lock().unwrap();
        let todo: Vec<Vec<u64>> = self
            .stats
            .tables
            .iter()
            .map(|ts| {
                let done = state.tables.get(&ts.name);
                let todo: Vec<u64> = (0..ts.batches_total).filter(|b| !done.is_some_and(|d| d.contains(*b))).collect();
                ts.resumed.store(state.rows(&ts.name), Ordering::Relaxed);
                ts.batches_left.store(todo.len() as u64, Ordering::Relaxed);
                if todo.is_empty() {
                    ts.finish();
                }
                todo
            })
            .collect();
        drop(state);

        let ordered = self.db.dialect().enforces_foreign_keys();
        let levels: Vec<Vec<usize>> = if ordered {
            self.plan.levels.iter().map(|level| level.iter().filter_map(|ti| self.stats_index(*ti)).collect()).collect()
        } else {
            vec![(0..self.stats.tables.len()).collect()]
        };
        let mut phases = Vec::new();
        for level in levels {
            let (sequential, parallel): (Vec<usize>, Vec<usize>) =
                level.into_iter().partition(|&t| ordered && self.plan.references_itself(self.stats.tables[t].index));
            phases.push(interleave(&parallel, &todo, self.config.generation.table_concurrency));
            for t in sequential {
                phases.extend(todo[t].iter().map(|&batch| vec![Unit { table: t, batch }]));
            }
        }
        phases.retain(|p| !p.is_empty());
        phases
    }

    fn stats_index(&self, schema_index: usize) -> Option<usize> {
        self.stats.tables.iter().position(|t| t.index == schema_index)
    }

    /// Loads one phase with the full pipeline.
    fn run_phase(&self, units: &[Unit]) -> Result<()> {
        let g = &self.config.generation;
        let cursor = AtomicUsize::new(0);
        let failure: Mutex<Option<Error>> = Mutex::new(None);
        let fail = |e: Error| {
            failure.lock().unwrap().get_or_insert(e);
            STOP.store(true, Ordering::SeqCst);
        };
        let (tx, rx) = crossbeam_channel::bounded::<Encoded>(self.config.queue_depth());
        let last_save = Mutex::new(Instant::now());
        let writers = g.workers.min(self.db.max_writers()).min(units.len()).max(1);
        let generators = self.config.generator_threads().min(units.len()).max(1);

        std::thread::scope(|scope| {
            for _ in 0..generators {
                let tx = tx.clone();
                let (cursor, fail) = (&cursor, &fail);
                scope.spawn(move || {
                    let mut row = Row::new();
                    while !STOP.load(Ordering::SeqCst) {
                        let Some(&unit) = units.get(cursor.fetch_add(1, Ordering::SeqCst)) else { return };
                        match self.encode_batch(unit, &mut row) {
                            Ok(enc) => {
                                if tx.send(enc).is_err() {
                                    return;
                                }
                            }
                            Err(e) => return fail(e),
                        }
                    }
                });
            }
            drop(tx);
            for _ in 0..writers {
                let rx = rx.clone();
                let (fail, last_save) = (&fail, &last_save);
                scope.spawn(move || {
                    // After a stop, the channel is drained without inserting: those
                    // batches are redone on resume.
                    while let Ok(enc) = rx.recv() {
                        if STOP.load(Ordering::SeqCst) {
                            continue;
                        }
                        match self.insert_batch(&enc) {
                            Ok(()) => self.record(&enc, last_save),
                            Err(e) => fail(e),
                        }
                    }
                });
            }
        });
        let saved = self.state.lock().unwrap().save(&self.state_path);
        if let Some(e) = failure.into_inner().unwrap() {
            return Err(e);
        }
        saved?;
        if STOP.load(Ordering::SeqCst) {
            return Err(Error::Interrupted);
        }
        Ok(())
    }

    fn encode_batch(&self, unit: Unit, row: &mut Row) -> Result<Encoded> {
        let ts = &self.stats.tables[unit.table];
        ts.start();
        let tg = self.plan.world.table(ts.index);
        let bs = self.config.generation.batch_size;
        let (start, end) = (unit.batch * bs, ((unit.batch + 1) * bs).min(tg.rows));
        let types: Vec<&DataType> = tg.insert.iter().map(|&c| &tg.cols[c].ty).collect();
        let mut batch = Batch { body: Vec::with_capacity(((end - start) as usize) * 64), row_ends: Vec::with_capacity((end - start) as usize) };
        let mut repaired = 0;
        for i in start..end {
            let steps = tg.row(&self.plan.world, i, row).map_err(|e| Error::Generation(e.to_string()))?;
            repaired += u64::from(steps > 0);
            let cells = RowRef { types: &types, values: &row.values, columns: &tg.insert };
            self.db.encode_row(&mut batch.body, &cells).map_err(|e| {
                let col = &tg.cols[tg.insert[e.column]];
                Error::Generation(format!("table {}, row {i}, column {}: {} (value {})", tg.name, col.name, e.message, row.values[tg.insert[e.column]]))
            })?;
            batch.row_ends.push(batch.body.len());
        }
        ts.repaired.fetch_add(repaired, Ordering::Relaxed);
        Ok(Encoded { unit, rows: end - start, batch })
    }

    fn insert_batch(&self, enc: &Encoded) -> Result<()> {
        let ts = &self.stats.tables[enc.unit.table];
        let tg = self.plan.world.table(ts.index);
        let table = &self.schema.tables[ts.index];
        let columns: Vec<&str> = tg.insert.iter().map(|&c| tg.cols[c].name.as_str()).collect();
        let max = self.config.generation.max_retries;
        let mut attempt = 0;
        loop {
            match self.db.insert(table, &columns, &enc.batch) {
                Ok(()) => {
                    ts.inserted.fetch_add(enc.rows, Ordering::Relaxed);
                    ts.bytes.fetch_add(enc.batch.body.len() as u64, Ordering::Relaxed);
                    return Ok(());
                }
                Err(e) if e.is_transient() && attempt < max => {
                    attempt += 1;
                    ts.retries.fetch_add(1, Ordering::Relaxed);
                    let wait = Duration::from_millis(500 * (1 << attempt.min(6)));
                    tracing::warn!(table = %ts.name, batch = enc.unit.batch, attempt, "retrying in {wait:?}: {e}");
                    std::thread::sleep(wait);
                }
                Err(e) => return Err(e.into_error(&format!("inserting batch {} of {}", enc.unit.batch, ts.name))),
            }
        }
    }

    /// Records a committed batch, saving the state at most once a second.
    fn record(&self, enc: &Encoded, last_save: &Mutex<Instant>) {
        let ts = &self.stats.tables[enc.unit.table];
        {
            let mut st = self.state.lock().unwrap();
            st.mark_done(&ts.name, enc.unit.batch, enc.rows);
            let mut last = last_save.lock().unwrap();
            if last.elapsed() > Duration::from_secs(1) {
                if let Err(e) = st.save(&self.state_path) {
                    tracing::warn!("{e}");
                }
                *last = Instant::now();
            }
        }
        if ts.batches_left.fetch_sub(1, Ordering::SeqCst) == 1 {
            ts.finish();
        }
    }
}

/// The batches of `tables`, in groups of `concurrency` tables whose batches alternate,
/// so tables loaded together progress together.
fn interleave(tables: &[usize], todo: &[Vec<u64>], concurrency: usize) -> Vec<Unit> {
    let mut units = Vec::new();
    for group in tables.chunks(concurrency.max(1)) {
        let longest = group.iter().map(|&t| todo[t].len()).max().unwrap_or(0);
        for k in 0..longest {
            units.extend(group.iter().filter_map(|&t| todo[t].get(k).map(|&batch| Unit { table: t, batch })));
        }
    }
    units
}
