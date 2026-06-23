//! Command-line interface — deliberately objdump-flavored.

use crate::progress::ProgressMode;
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// Version shown by `--version`: the crate version plus the git commit that
/// built it (e.g. `0.1.0 (a1b2c3d)`), as wired up by `build.rs`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), env!("SRCDUMP_GIT_DESC"));

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

/// When to syntax-highlight the plain (non-TUI) dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Color when stdout is a terminal; plain otherwise.
    Auto,
    /// Always color.
    Always,
    /// Never color.
    Never,
}

/// objdump-like tool that displays decompiled code instead of disassembly.
///
/// By default `srcdump FILE` decompiles every recovered function in the
/// binary, fanning the work out across one Python/angr worker per CPU.
#[derive(Debug, Parser)]
#[command(name = "srcdump", version = VERSION, about, long_about = None)]
pub struct Cli {
    /// The binary to decompile.
    #[arg(required_unless_present = "remove_env")]
    pub file: Option<PathBuf>,

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

    /// Number of parallel worker processes (default: number of cores).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Enable angr's auto_load_libs (off by default for speed).
    #[arg(long = "load-libs")]
    pub load_libs: bool,

    /// Include PLT stubs / thunks in the output.
    #[arg(long = "include-plt")]
    pub include_plt: bool,

    /// Rebuild the cached CFG (and clear cached decompilation results).
    #[arg(long = "rebuild")]
    pub rebuild: bool,

    /// Re-decompile every function, ignoring (and overwriting) the
    /// decompilation cache. Keeps the cached CFG.
    #[arg(long = "redecompile")]
    pub redecompile: bool,

    /// Directory for the CFG cache (default: $XDG_CACHE_HOME/srcdump or
    /// ~/.cache/srcdump).
    #[arg(long = "cache-dir", value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Use an existing virtualenv that already has angr (skips the managed
    /// env and uv entirely). Expects an interpreter at <DIR>/bin/python.
    #[arg(long = "venv", value_name = "DIR")]
    pub venv: Option<PathBuf>,

    /// Location of the srcdump-managed Python env (default:
    /// $XDG_DATA_HOME/srcdump/env or ~/.local/share/srcdump/env). Override via
    /// $SRCDUMP_ENV_DIR.
    #[arg(long = "env-dir", value_name = "DIR")]
    pub env_dir: Option<PathBuf>,

    /// Provision the managed env from this prebuilt zip instead of uv. By
    /// default, a `uv_angr.zip` next to the srcdump binary is used if present.
    #[arg(long = "env-archive", value_name = "FILE")]
    pub env_archive: Option<PathBuf>,

    /// Recreate / upgrade the managed angr env.
    #[arg(long = "refresh-env")]
    pub refresh_env: bool,

    /// Remove the srcdump-managed Python env and exit (honors --env-dir).
    #[arg(long = "remove-env")]
    pub remove_env: bool,

    /// Skip confirmation prompts (e.g. for --remove-env).
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,

    /// Python version for the uv-managed env.
    #[arg(long = "python-version", value_name = "X.Y", default_value = "3.14")]
    pub python_version: String,

    /// Low-level override: exact Python interpreter to run workers with
    /// (skips the managed env). Also read from $SRCDUMP_PYTHON.
    #[arg(long = "python", value_name = "PATH")]
    pub python: Option<PathBuf>,

    /// When to show the progress display on stderr.
    #[arg(long = "progress", value_enum, default_value_t = ProgressMode::Auto)]
    pub progress: ProgressMode,

    /// Interactive TUI browser (default: auto = on when stdout is a terminal).
    /// Ignored with --list or --slowest.
    #[arg(long = "tui", value_enum, default_value_t = TuiMode::Auto)]
    pub tui: TuiMode,

    /// Syntax-highlight the plain dump (default: auto = on when stdout is a
    /// terminal). The TUI is always highlighted.
    #[arg(long = "color", value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,
}

impl Cli {
    /// Effective worker count: requested, else CPU count, clamped to >= 1.
    pub fn job_count(&self) -> usize {
        self.jobs.unwrap_or_else(num_cpus::get).max(1)
    }
}
