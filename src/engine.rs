//! The load pipeline: generator threads produce encoded batches, writer threads insert
//! them, each batch in one request; committed batches are recorded for resuming.
//!
//! ```text
//!   work queue ──> generator threads ──(bounded channel)──> writer threads ──> database
//!   (table, batch)    rows + encoding         queue_depth          INSERT, retries
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::gen::Row;
use crate::plan::Plan;
use crate::schema::Schema;
use crate::state::State;

/// Live counters of one table.
pub struct TableStats {
    pub name: String,
    pub index: usize,
    pub target: u64,
    pub inserted: AtomicU64,
    pub resumed: AtomicU64,
    pub bytes: AtomicU64,
    pub retries: AtomicU64,
    pub repaired: AtomicU64,
    pub batches_total: u64,
    batches_left: AtomicU64,
    started: Mutex<Option<Instant>>,
    finished: Mutex<Option<Instant>>,
}

impl TableStats {
    pub fn done(&self) -> u64 {
        self.inserted.load(Ordering::Relaxed) + self.resumed.load(Ordering::Relaxed)
    }

    pub fn is_started(&self) -> bool {
        self.started.lock().unwrap().is_some()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.lock().unwrap().is_some()
    }

    pub fn elapsed(&self) -> Option<Duration> {
        let start = (*self.started.lock().unwrap())?;
        Some(self.finished.lock().unwrap().unwrap_or_else(Instant::now) - start)
    }

    fn start(&self) {
        let mut s = self.started.lock().unwrap();
        if s.is_none() {
            *s = Some(Instant::now());
        }
    }
}

pub struct Stats {
    pub tables: Vec<TableStats>,
}

impl Stats {
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

    pub fn target(&self) -> u64 {
        self.tables.iter().map(|t| t.target).sum()
    }

    pub fn done(&self) -> u64 {
        self.tables.iter().map(TableStats::done).sum()
    }
}

/// Set by the first Ctrl-C: stop taking new batches, let in-flight ones finish.
pub static STOP: AtomicBool = AtomicBool::new(false);

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

pub struct Load<'a> {
    pub config: &'a Config,
    pub schema: &'a Schema,
    pub plan: &'a Plan,
    pub db: &'a Database,
    pub stats: Arc<Stats>,
    pub state: Arc<Mutex<State>>,
    pub state_path: PathBuf,
}

struct Unit {
    table: usize,
    batch: u64,
}

struct Encoded {
    table: usize,
    batch: u64,
    rows: u64,
    body: Vec<u8>,
}

impl Load<'_> {
    /// Runs the load. Returns `Err(Interrupted)` after a Ctrl-C, having recorded every
    /// committed batch.
    pub fn run(&self) -> Result<()> {
        let g = &self.config.generation;
        let bs = g.batch_size;
        // Batches still to do, tables grouped by `table_concurrency` and interleaved within
        // a group, parents first.
        let mut units = Vec::new();
        {
            let state = self.state.lock().unwrap();
            for group in (0..self.stats.tables.len()).collect::<Vec<_>>().chunks(g.table_concurrency) {
                let mut per_table: Vec<Vec<u64>> = Vec::new();
                for &t in group {
                    let ts = &self.stats.tables[t];
                    let done = state.tables.get(&ts.name);
                    let todo: Vec<u64> = (0..ts.batches_total).filter(|b| !done.is_some_and(|d| d.contains(*b))).collect();
                    let resumed = state.rows(&ts.name);
                    ts.resumed.store(resumed, Ordering::Relaxed);
                    ts.batches_left.store(todo.len() as u64, Ordering::Relaxed);
                    if todo.is_empty() {
                        ts.start();
                        *ts.finished.lock().unwrap() = Some(Instant::now());
                    }
                    per_table.push(todo);
                }
                let longest = per_table.iter().map(Vec::len).max().unwrap_or(0);
                for k in 0..longest {
                    for (j, &t) in group.iter().enumerate() {
                        if let Some(&b) = per_table[j].get(k) {
                            units.push(Unit { table: t, batch: b });
                        }
                    }
                }
            }
        }
        if units.is_empty() {
            return Ok(());
        }
        let cursor = AtomicUsize::new(0);
        let failure: Mutex<Option<Error>> = Mutex::new(None);
        let fail = |e: Error| {
            let mut f = failure.lock().unwrap();
            if f.is_none() {
                *f = Some(e);
            }
            STOP.store(true, Ordering::SeqCst);
        };
        let (tx, rx) = crossbeam_channel::bounded::<Encoded>(self.config.queue_depth());
        let last_save = Mutex::new(Instant::now());
        let interrupted = AtomicBool::new(false);

        std::thread::scope(|scope| {
            for _ in 0..self.config.generator_threads() {
                let tx = tx.clone();
                let (cursor, units, fail) = (&cursor, &units, &fail);
                scope.spawn(move || {
                    let mut row = Row::new();
                    loop {
                        if STOP.load(Ordering::SeqCst) {
                            return;
                        }
                        let k = cursor.fetch_add(1, Ordering::SeqCst);
                        let Some(unit) = units.get(k) else { return };
                        match self.encode_batch(unit, bs, &mut row) {
                            Ok(enc) => {
                                if tx.send(enc).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                fail(e);
                                return;
                            }
                        }
                    }
                });
            }
            drop(tx);
            for _ in 0..g.workers {
                let rx = rx.clone();
                let (fail, last_save, interrupted) = (&fail, &last_save, &interrupted);
                scope.spawn(move || {
                    while let Ok(enc) = rx.recv() {
                        if STOP.load(Ordering::SeqCst) {
                            interrupted.store(true, Ordering::SeqCst);
                            continue; // drain without inserting: those batches are redone on resume
                        }
                        if let Err(e) = self.insert_batch(&enc) {
                            fail(e);
                            continue;
                        }
                        let ts = &self.stats.tables[enc.table];
                        {
                            let mut st = self.state.lock().unwrap();
                            st.mark_done(&ts.name, enc.batch, enc.rows);
                            let mut last = last_save.lock().unwrap();
                            if last.elapsed() > Duration::from_secs(1) {
                                if let Err(e) = st.save(&self.state_path) {
                                    tracing::warn!("{e}");
                                }
                                *last = Instant::now();
                            }
                        }
                        if ts.batches_left.fetch_sub(1, Ordering::SeqCst) == 1 {
                            *ts.finished.lock().unwrap() = Some(Instant::now());
                        }
                    }
                });
            }
        });
        let save = self.state.lock().unwrap().save(&self.state_path);
        if let Some(e) = failure.into_inner().unwrap() {
            return Err(e);
        }
        save?;
        if STOP.load(Ordering::SeqCst) || interrupted.load(Ordering::SeqCst) {
            return Err(Error::Interrupted);
        }
        Ok(())
    }

    fn encode_batch(&self, unit: &Unit, bs: u64, row: &mut Row) -> Result<Encoded> {
        let ts = &self.stats.tables[unit.table];
        ts.start();
        let tg = self.plan.world.table(ts.index);
        let start = unit.batch * bs;
        let end = (start + bs).min(tg.rows);
        let mut body = Vec::with_capacity(((end - start) as usize) * 64);
        let mut repaired = 0;
        for i in start..end {
            let steps = tg.row(&self.plan.world, i, row).map_err(|e| Error::Generation(e.to_string()))?;
            if steps > 0 {
                repaired += 1;
            }
            for &c in &tg.insert {
                let col = &tg.cols[c];
                self.db.encode(&mut body, &col.ty, &row.values[c]).map_err(|e| {
                    Error::Generation(format!("table {}, row {i}, column {}: {e} (value {})", tg.name, col.name, row.values[c]))
                })?;
            }
        }
        ts.repaired.fetch_add(repaired, Ordering::Relaxed);
        Ok(Encoded { table: unit.table, batch: unit.batch, rows: end - start, body })
    }

    fn insert_batch(&self, enc: &Encoded) -> Result<()> {
        let ts = &self.stats.tables[enc.table];
        let tg = self.plan.world.table(ts.index);
        let table = &self.schema.tables[ts.index];
        let columns: Vec<&str> = tg.insert.iter().map(|&c| tg.cols[c].name.as_str()).collect();
        let max = self.config.generation.max_retries;
        let mut attempt = 0;
        loop {
            match self.db.insert(table, &columns, &enc.body) {
                Ok(()) => {
                    ts.inserted.fetch_add(enc.rows, Ordering::Relaxed);
                    ts.bytes.fetch_add(enc.body.len() as u64, Ordering::Relaxed);
                    return Ok(());
                }
                Err(e) if e.transient && attempt < max => {
                    attempt += 1;
                    ts.retries.fetch_add(1, Ordering::Relaxed);
                    let wait = Duration::from_millis(500 * (1 << attempt.min(6)));
                    tracing::warn!(table = %ts.name, batch = enc.batch, attempt, "retrying in {:?}: {e}", wait);
                    std::thread::sleep(wait);
                }
                Err(e) => return Err(e.into_error(&format!("inserting batch {} of {}", enc.batch, ts.name))),
            }
        }
    }
}
