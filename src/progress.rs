//! Live progress: bars on a terminal, periodic log lines otherwise (CI, nohup, `docker
//! logs`). Log output goes through [`LogWriter`] so it never tears the bars.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::engine::Stats;
use crate::report::thousands;
use crate::timeutil::format_duration;

static BARS: OnceLock<MultiProgress> = OnceLock::new();

/// `tracing` writer that prints above the progress bars when they are shown.
pub struct LogWriter;

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match BARS.get() {
            Some(bars) => bars.suspend(|| std::io::stderr().write(buf)),
            None => std::io::stderr().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// The progress display, running on its own thread until [`Progress::stop`].
pub struct Progress {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Progress {
    /// `bars`: draw bars if stderr is a terminal; otherwise log every `interval`.
    pub fn start(stats: Arc<Stats>, bars: bool, interval: Duration) -> Progress {
        let stop = Arc::new(AtomicBool::new(false));
        let interactive = bars && std::io::stderr().is_terminal();
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            if interactive {
                draw_bars(&stats, &flag);
            } else {
                log_lines(&stats, interval, &flag);
            }
        });
        Progress { stop, thread: Some(thread) }
    }

    /// Stops the display and waits for its last update.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn draw_bars(stats: &Stats, stop: &AtomicBool) {
    let multi = BARS.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(4)));
    let style = ProgressStyle::with_template("{prefix:<30.cyan} [{bar:30.green/blue}] {human_pos:>12}/{human_len:<12} {per_sec:>12} eta {eta}")
        .expect("valid template")
        .progress_chars("=> ");
    let total = multi.add(ProgressBar::new(stats.target()));
    total.set_style(style.clone());
    total.set_prefix("TOTAL");
    let mut bars: Vec<Option<ProgressBar>> = vec![None; stats.tables.len()];
    loop {
        for (i, t) in stats.tables.iter().enumerate() {
            match &bars[i] {
                None if t.is_started() && !t.is_finished() => {
                    let bar = multi.insert_before(&total, ProgressBar::new(t.target));
                    bar.set_style(style.clone());
                    bar.set_prefix(t.name.clone());
                    bar.set_position(t.done());
                    bars[i] = Some(bar);
                }
                Some(bar) => {
                    bar.set_position(t.done());
                    if t.is_finished() && !bar.is_finished() {
                        bar.finish_and_clear();
                    }
                }
                None => {}
            }
        }
        total.set_position(stats.done());
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for bar in bars.into_iter().flatten() {
        bar.finish_and_clear();
    }
    total.finish_and_clear();
    let _ = multi.clear();
}

fn log_lines(stats: &Stats, interval: Duration, stop: &AtomicBool) {
    let mut last_done = stats.done();
    let mut last = Instant::now();
    loop {
        let wake = Instant::now() + interval;
        while Instant::now() < wake {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let done = stats.done();
        let now = Instant::now();
        let rate = (done - last_done) as f64 / (now - last).as_secs_f64().max(1e-3);
        let remaining = stats.target().saturating_sub(done);
        let eta = if rate > 0.0 { format_duration(remaining as f64 / rate) } else { "-".into() };
        let active: Vec<String> = stats
            .tables
            .iter()
            .filter(|t| t.is_started() && !t.is_finished())
            .map(|t| format!("{} {:.0}%", t.name, 100.0 * t.done() as f64 / t.target.max(1) as f64))
            .collect();
        tracing::info!(
            done = %thousands(done),
            target = %thousands(stats.target()),
            percent = format!("{:.1}", 100.0 * done as f64 / stats.target().max(1) as f64),
            rows_per_sec = format!("{rate:.0}"),
            eta = %eta,
            active = %active.join(", "),
            "progress"
        );
        last_done = done;
        last = now;
    }
}
