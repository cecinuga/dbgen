//! Resume state: which batches of which tables are in the database. Kept in a JSON file
//! next to the configuration, one per target database.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};

/// Version of the state file layout.
pub const FORMAT: u32 = 1;

/// What a load has committed, and the settings it ran with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub format: u32,
    pub dbgen_version: String,
    /// Identifies the data: same fingerprint, same rows (see `Plan::fingerprint`).
    pub fingerprint: String,
    pub database: String,
    pub seed: u64,
    pub batch_size: u64,
    pub window: (String, String),
    pub targets: BTreeMap<String, u64>,
    pub tables: BTreeMap<String, TableState>,
    pub completed: bool,
    pub started_at: String,
    pub updated_at: String,
}

/// The committed batches of one table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableState {
    /// Committed batch indices, as `[first, last]` ranges.
    batches: Vec<(u64, u64)>,
    pub rows: u64,
    #[serde(skip)]
    set: BTreeSet<u64>,
}

impl TableState {
    fn expand(&mut self) {
        self.set = self.batches.iter().flat_map(|&(a, b)| a..=b).collect();
    }

    fn compress(&mut self) {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for &b in &self.set {
            match ranges.last_mut() {
                Some((_, last)) if *last + 1 == b => *last = b,
                _ => ranges.push((b, b)),
            }
        }
        self.batches = ranges;
    }

    /// Whether batch `batch` is committed.
    pub fn contains(&self, batch: u64) -> bool {
        self.set.contains(&batch)
    }
}

/// Where the state of the target `state_key` is kept.
pub fn path_for(config: &Config, state_key: &str) -> PathBuf {
    match &config.generation.state_file {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => config.base_dir.join(p),
        None => config.base_dir.join(".dbgen").join(format!("{state_key}.json")),
    }
}

impl State {
    /// Reads a state file; `None` when there is none.
    pub fn load(path: &Path) -> Result<Option<State>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Config(format!("cannot read resume state {}: {e}", path.display()))),
        };
        let mut s: State = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("resume state {} is corrupt ({e}); delete it and use --fresh", path.display())))?;
        if s.format != FORMAT {
            return Err(Error::Config(format!(
                "resume state {} has format {}, this dbgen writes {FORMAT}; delete it and use --fresh",
                path.display(),
                s.format
            )));
        }
        for t in s.tables.values_mut() {
            t.expand();
        }
        Ok(Some(s))
    }

    /// Writes the state atomically (to a temporary file, then renamed).
    pub fn save(&mut self, path: &Path) -> Result<()> {
        for t in self.tables.values_mut() {
            t.compress();
        }
        self.updated_at = chrono::Utc::now().to_rfc3339();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Config(format!("cannot create {}: {e}", dir.display())))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).expect("serializable");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, path))
            .map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))
    }

    /// The state of table `name`, created empty if needed.
    pub fn table(&mut self, name: &str) -> &mut TableState {
        self.tables.entry(name.to_string()).or_default()
    }

    /// Rows committed in table `name`.
    pub fn rows(&self, name: &str) -> u64 {
        self.tables.get(name).map_or(0, |t| t.rows)
    }

    /// Records a committed batch of `rows` rows.
    pub fn mark_done(&mut self, table: &str, batch: u64, rows: u64) {
        let t = self.table(table);
        if t.set.insert(batch) {
            t.rows += rows;
        }
    }

    /// Drops what is recorded for `table`, which will be loaded again.
    pub fn forget(&mut self, table: &str) {
        self.tables.remove(table);
    }

    /// Why a resume with these parameters is refused, if it is.
    pub fn differences(&self, other: &State) -> Vec<String> {
        let mut out = Vec::new();
        if self.seed != other.seed {
            out.push(format!("seed: {} -> {}", self.seed, other.seed));
        }
        if self.batch_size != other.batch_size {
            out.push(format!("batch_size: {} -> {}", self.batch_size, other.batch_size));
        }
        if self.window != other.window {
            out.push(format!("window: {} .. {} -> {} .. {}", self.window.0, self.window.1, other.window.0, other.window.1));
        }
        for (t, n) in &other.targets {
            match self.targets.get(t) {
                Some(m) if m != n => out.push(format!("tables.{t}: {m} -> {n}")),
                None => out.push(format!("tables.{t}: added ({n})")),
                _ => {}
            }
        }
        for t in self.targets.keys() {
            if !other.targets.contains_key(t) {
                out.push(format!("tables.{t}: removed"));
            }
        }
        if out.is_empty() && self.fingerprint != other.fingerprint {
            out.push("the schema, the relations or the column settings changed".into());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_round_trip_as_ranges() {
        let mut t = TableState::default();
        for b in [0, 1, 2, 5, 7, 8] {
            t.set.insert(b);
        }
        t.compress();
        assert_eq!(t.batches, vec![(0, 2), (5, 5), (7, 8)]);
        let mut u = TableState { batches: t.batches.clone(), ..Default::default() };
        u.expand();
        assert!(u.contains(1) && u.contains(8) && !u.contains(3));
    }
}
