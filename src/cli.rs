//! Command-line interface — deliberately objdump-flavored.

use clap::Parser;
use std::path::PathBuf;

/// objdump-like tool that displays decompiled code instead of disassembly.
///
/// By default `sourcedump FILE` decompiles every recovered function in the
/// binary, fanning the work out across one Python/angr worker per CPU.
#[derive(Debug, Parser)]
#[command(name = "sourcedump", version, about, long_about = None)]
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

    /// Number of parallel worker processes (default: number of CPUs).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Enable angr's auto_load_libs (off by default for speed).
    #[arg(long = "load-libs")]
    pub load_libs: bool,

    /// Include PLT stubs / thunks in the output.
    #[arg(long = "include-plt")]
    pub include_plt: bool,

    /// Python interpreter to run workers with. Falls back to
    /// $SOURCEDUMP_PYTHON, then $VIRTUAL_ENV/bin/python, then `python3`.
    #[arg(long = "python", value_name = "PATH")]
    pub python: Option<String>,
}

impl Cli {
    /// Effective worker count: requested, else CPU count, clamped to >= 1.
    pub fn job_count(&self) -> usize {
        self.jobs.unwrap_or_else(num_cpus::get).max(1)
    }
}
