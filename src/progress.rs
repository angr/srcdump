//! Live progress display on stderr.
//!
//! The reporter renders a single, in-place updating status line that walks
//! through the pipeline stages: loading the binary, building the CFG, then
//! decompiling functions with an `N / total` counter and an ETA. It writes only
//! to stderr (so redirected stdout stays clean) and only when enabled.

use std::io::{IsTerminal, Write};
use std::time::Instant;

/// When to show the progress line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProgressMode {
    /// Show only when stderr is a terminal.
    Auto,
    /// Always show.
    Always,
    /// Never show.
    Never,
}

impl ProgressMode {
    fn enabled(self) -> bool {
        match self {
            ProgressMode::Always => true,
            ProgressMode::Never => false,
            ProgressMode::Auto => std::io::stderr().is_terminal(),
        }
    }
}

/// The pipeline stage currently in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Loading,
    BuildingCfg,
    Working,
    Saving,
}

/// Minimum wall-clock gap between redraws, to avoid terminal flicker.
const REDRAW_INTERVAL_MS: u128 = 75;
const BAR_WIDTH: usize = 24;

pub struct Progress {
    enabled: bool,
    /// Label for the work stage ("Decompiling" vs "Listing").
    work_verb: &'static str,
    stage: Stage,
    cfg_percent: u8,
    total: usize,
    done: usize,
    started: Instant,
    /// When the work stage began (for ETA). Set on first entry to `Working`.
    work_started: Option<Instant>,
    last_draw: Option<Instant>,
    /// Width of the last line drawn, so we can clear it fully.
    last_len: usize,
}

impl Progress {
    pub fn new(mode: ProgressMode, is_list: bool) -> Self {
        Self {
            enabled: mode.enabled(),
            work_verb: if is_list { "Listing" } else { "Decompiling" },
            stage: Stage::Loading,
            cfg_percent: 0,
            total: 0,
            done: 0,
            started: Instant::now(),
            work_started: None,
            last_draw: None,
            last_len: 0,
        }
    }

    /// Build-worker progress message (`load` / `cfg` / `save`).
    pub fn build_stage(&mut self, stage: &str, percent: Option<f64>) {
        match stage {
            "load" => self.stage = Stage::Loading,
            "cfg" => {
                self.stage = Stage::BuildingCfg;
                if let Some(p) = percent {
                    self.cfg_percent = p.clamp(0.0, 100.0) as u8;
                }
            }
            "save" => self.stage = Stage::Saving,
            _ => {}
        }
        self.draw(false);
    }

    /// Enter the decompile/list stage (after the CFG cache is ready).
    pub fn begin_work(&mut self) {
        self.stage = Stage::Working;
        self.work_started.get_or_insert_with(Instant::now);
        self.draw(true);
    }

    /// Add a worker's stripe size to the grand total.
    pub fn add_total(&mut self, n: usize) {
        self.total += n;
        self.draw(false);
    }

    /// Record one completed function.
    pub fn inc_done(&mut self) {
        self.done += 1;
        self.draw(false);
    }

    /// Clear the line and print a one-line summary.
    pub fn finish(&mut self, summary: &str) {
        if !self.enabled {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{summary}\n");
        let _ = err.flush();
    }

    /// Render the current state, throttled unless `force`.
    fn draw(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_draw {
                if now.duration_since(last).as_millis() < REDRAW_INTERVAL_MS {
                    return;
                }
            }
        }
        self.last_draw = Some(now);

        let line = match self.stage {
            Stage::Loading => "Loading binary…".to_string(),
            Stage::BuildingCfg => format!("Building CFG  {:>3}%", self.cfg_percent),
            Stage::Saving => "Saving CFG cache…".to_string(),
            Stage::Working => self.work_line(),
        };

        let mut err = std::io::stderr().lock();
        // Carriage return, write, then pad to erase any leftover of a longer
        // previous line.
        let _ = write!(err, "\r{line}");
        if self.last_len > line.chars().count() {
            let pad = self.last_len - line.chars().count();
            let _ = write!(err, "{:pad$}", "", pad = pad);
        }
        let _ = err.flush();
        self.last_len = line.chars().count();
    }

    fn work_line(&self) -> String {
        if self.total == 0 {
            return format!("{} (starting workers…)", self.work_verb);
        }
        let frac = (self.done as f64 / self.total as f64).clamp(0.0, 1.0);
        let filled = (frac * BAR_WIDTH as f64).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
        format!(
            "{} {}/{}  [{}] {:>3}%  ETA {}",
            self.work_verb,
            self.done,
            self.total,
            bar,
            (frac * 100.0) as u8,
            self.eta(),
        )
    }

    /// Estimated time remaining, from the observed completion rate.
    fn eta(&self) -> String {
        let Some(started) = self.work_started else {
            return "--:--".to_string();
        };
        if self.done == 0 || self.done >= self.total {
            return "--:--".to_string();
        }
        let elapsed = started.elapsed().as_secs_f64();
        let rate = self.done as f64 / elapsed; // functions per second
        if rate <= 0.0 {
            return "--:--".to_string();
        }
        let remaining = (self.total - self.done) as f64 / rate;
        format_duration(remaining)
    }

    /// Total wall-clock since construction, formatted.
    pub fn elapsed_str(&self) -> String {
        format_duration(self.started.elapsed().as_secs_f64())
    }
}

/// Format a duration in seconds as `M:SS` (or `H:MM:SS` past an hour).
fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
