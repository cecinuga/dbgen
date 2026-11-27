//! End-of-run summary (terminal) and machine-readable report (JSON, for comparing a
//! series of runs).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use serde::Serialize;

use crate::engine::Stats;
use crate::timeutil::format_duration;

/// How one table ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TableStatus {
    /// Final row count equals the target.
    Success,
    /// The load finished but the table does not hold exactly the target.
    Mismatch,
    /// The run stopped before the table was complete.
    Incomplete,
}

/// The outcome of one table.
#[derive(Debug, Clone, Serialize)]
pub struct TableResult {
    pub table: String,
    pub target_rows: u64,
    pub inserted_rows: u64,
    pub resumed_rows: u64,
    pub final_rows: Option<u64>,
    pub repaired_rows: u64,
    pub seconds: f64,
    pub rows_per_sec: f64,
    pub mib: f64,
    pub retries: u64,
    pub status: TableStatus,
}

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OverallStatus {
    Success,
    Failed,
    Interrupted,
}

/// The outcome of a load: printed at the end, and written by `--report`.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub database: String,
    pub server_version: String,
    pub fingerprint: String,
    pub seed: u64,
    pub window_start: String,
    pub window_end: String,
    pub batch_size: u64,
    pub workers: usize,
    pub tables: Vec<TableResult>,
    pub load_seconds: f64,
    pub total_seconds: f64,
    pub status: OverallStatus,
    pub error: Option<String>,
}

impl Summary {
    /// Per-table results from the live counters and the exact counts taken after the load.
    pub fn tables_from(stats: &Stats, finals: &[Option<u64>], interrupted: bool) -> Vec<TableResult> {
        stats
            .tables
            .iter()
            .zip(finals)
            .map(|(t, &final_rows)| {
                let inserted = t.inserted.load(Ordering::Relaxed);
                let seconds = t.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let status = match final_rows {
                    Some(n) if n == t.target => TableStatus::Success,
                    _ if interrupted || t.done() < t.target => TableStatus::Incomplete,
                    _ => TableStatus::Mismatch,
                };
                TableResult {
                    table: t.name.clone(),
                    target_rows: t.target,
                    inserted_rows: inserted,
                    resumed_rows: t.resumed.load(Ordering::Relaxed),
                    final_rows,
                    repaired_rows: t.repaired.load(Ordering::Relaxed),
                    seconds,
                    rows_per_sec: if seconds > 0.0 { inserted as f64 / seconds } else { 0.0 },
                    mib: t.bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
                    retries: t.retries.load(Ordering::Relaxed),
                    status,
                }
            })
            .collect()
    }

    /// The summary as a text table.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let rule = "-".repeat(118);
        let _ = writeln!(s, "\nDBGEN LOAD SUMMARY\n");
        let _ = writeln!(s, "Database: {}   Server: {}", self.database, self.server_version);
        let _ = writeln!(s, "Seed: {}   Window: {} -> {}   Fingerprint: {}", self.seed, self.window_start, self.window_end, self.fingerprint);
        let _ = writeln!(s, "Batch size: {}   Workers: {}\n", self.batch_size, self.workers);
        let _ = writeln!(
            s,
            "{:<32} {:>13} {:>15} {:>13} {:>10} {:>9} {:>11} {:>7}  Status",
            "Table", "Target Rows", "Inserted Rows", "Final Rows", "Repaired", "Duration", "Rows/s", "Retries"
        );
        let _ = writeln!(s, "{rule}");
        for t in &self.tables {
            let inserted =
                if t.resumed_rows > 0 { format!("{}+{}", thousands(t.inserted_rows), thousands(t.resumed_rows)) } else { thousands(t.inserted_rows) };
            let _ = writeln!(
                s,
                "{:<32} {:>13} {:>15} {:>13} {:>10} {:>9} {:>11} {:>7}  {:?}",
                t.table,
                thousands(t.target_rows),
                inserted,
                t.final_rows.map_or("?".into(), thousands),
                thousands(t.repaired_rows),
                format_duration(t.seconds),
                if t.rows_per_sec > 0.0 { thousands(t.rows_per_sec as u64) } else { "-".into() },
                t.retries,
                t.status
            );
        }
        let _ = writeln!(s, "{rule}");
        let target: u64 = self.tables.iter().map(|t| t.target_rows).sum();
        let inserted: u64 = self.tables.iter().map(|t| t.inserted_rows).sum();
        let resumed: u64 = self.tables.iter().map(|t| t.resumed_rows).sum();
        let finals: u64 = self.tables.iter().filter_map(|t| t.final_rows).sum();
        let mib: f64 = self.tables.iter().map(|t| t.mib).sum();
        let _ = writeln!(s, "Total Target Rows:    {}", thousands(target));
        let _ = writeln!(
            s,
            "Total Inserted Rows:  {}{}",
            thousands(inserted),
            if resumed > 0 { format!(" (+{} resumed)", thousands(resumed)) } else { String::new() }
        );
        let _ = writeln!(s, "Total Final Rows:     {}", thousands(finals));
        let _ = writeln!(
            s,
            "Load Throughput:      {} rows/s, {:.1} MiB/s",
            thousands((inserted as f64 / self.load_seconds.max(1e-3)) as u64),
            mib / self.load_seconds.max(1e-3)
        );
        let _ = writeln!(s, "Load Time:            {}", format_duration(self.load_seconds));
        let _ = writeln!(s, "Execution Time:       {}", format_duration(self.total_seconds));
        let _ = writeln!(s, "Overall Status:       {:?}", self.status);
        if let Some(e) = &self.error {
            let _ = writeln!(s, "Error:                {e}");
        }
        s
    }
}

/// `1234567` -> `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(1_000), "1,000");
        assert_eq!(super::thousands(8_150_000), "8,150,000");
    }
}
