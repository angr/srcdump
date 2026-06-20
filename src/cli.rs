//! Command-line interface — deliberately objdump-flavored.

use crate::progress::ProgressMode;
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// When to use the interactive TUI instead of dumping to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TuiMode {
    /// Interactive TUI when stdout is a terminal; plain dump otherwise.
    Auto,
    /// Always use the interactive TUI.
    Always,
    /// Never use the TUI; always dump.
    Never,
}

/// objdump-like tool that displays decompiled code instead of disassembly.
///
/// By default `srcdump FILE` decompiles every recovered function in the
/// binary, fanning the work out across one Python/angr worker per CPU.
#[derive(Debug, Parser)]
#[command(name = "srcdump", version, about, long_about = None)]
pub struct Cli {
    /// The binary to decompile.
    pub file: PathBuf,

    /// Only decompile these functions (by name or 0x-prefixed address).
    /// May be repeated. With no `-f`, the whole binary is decompiled.
    #[arg(short = 'f', long = "function", value_name = "NAME|ADDR")]
    pub functions: Vec<String>,

    /// List functions instead of decompiling them (objdump -t flavored).
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    /// Report the N slowest functions by decompilation time (descending),
    /// instead of printing decompiled code.
    #[arg(long = "slowest", value_name = "N", conflicts_with = "list")]
    pub slowest: Option<usize>,

    /// Number of parallel worker processes (default: number of CPUs).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Enable angr's auto_load_libs (off by default for speed).
    #[arg(long = "load-libs")]
    pub load_libs: bool,

    /// Include PLT stubs / thunks in the output.
    #[arg(long = "include-plt")]
    pub include_plt: bool,

    /// Rebuild the cached CFG even if a cached angrdb already exists.
    #[arg(long = "rebuild")]
    pub rebuild: bool,

    /// Directory for the CFG cache (default: $XDG_CACHE_HOME/srcdump or
    /// ~/.cache/srcdump).
    #[arg(long = "cache-dir", value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Python interpreter to run workers with. Falls back to
    /// $SRCDUMP_PYTHON, then $VIRTUAL_ENV/bin/python, then `python3`.
    #[arg(long = "python", value_name = "PATH")]
    pub python: Option<String>,

    /// When to show the progress display on stderr.
    #[arg(long = "progress", value_enum, default_value_t = ProgressMode::Auto)]
    pub progress: ProgressMode,

    /// Interactive TUI browser (default: auto = on when stdout is a terminal).
    /// Ignored with --list or --slowest.
    #[arg(long = "tui", value_enum, default_value_t = TuiMode::Auto)]
    pub tui: TuiMode,
}

impl Cli {
    /// Effective worker count: requested, else CPU count, clamped to >= 1.
    pub fn job_count(&self) -> usize {
        self.jobs.unwrap_or_else(num_cpus::get).max(1)
    }
}
