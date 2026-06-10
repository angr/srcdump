//! sourcedump — an objdump-like front end that prints decompiled code.
//!
//! The binary is a pure orchestrator: it shards decompilation across a pool of
//! Python/angr worker processes and merges their output. See `worker.py` for
//! the per-worker logic and `pool.rs` for the process management.

mod backend;
mod cli;
mod model;
mod output;
mod pool;

use anyhow::{Context, Result};
use backend::{DecompileBackend, DecompileRequest, Mode};
use clap::Parser;
use cli::Cli;
use pool::ProcessPoolBackend;
use std::io::{self, Write};

fn main() {
    if let Err(err) = run() {
        eprintln!("sourcedump: error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if !cli.file.exists() {
        anyhow::bail!("no such file: {}", cli.file.display());
    }

    let python = resolve_python(cli.python.as_deref());
    let mode = if cli.list { Mode::List } else { Mode::Decompile };

    let req = DecompileRequest {
        binary: cli.file.clone(),
        functions: cli.functions.clone(),
        load_libs: cli.load_libs,
        include_plt: cli.include_plt,
        jobs: cli.job_count(),
        mode,
        python,
    };

    let backend = ProcessPoolBackend::new().context("initializing worker pool")?;
    let result = backend.run(&req)?;

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match mode {
        Mode::List => output::print_listing(&mut out, &cli.file, &result)?,
        Mode::Decompile => output::print_decompilation(&mut out, &cli.file, &result)?,
    }
    out.flush()?;
    Ok(())
}

/// Resolve the Python interpreter used to run workers.
///
/// Precedence: `--python` flag, then `$SOURCEDUMP_PYTHON`, then an active
/// virtualenv (`$VIRTUAL_ENV/bin/python`), then plain `python3` on PATH.
fn resolve_python(flag: Option<&str>) -> String {
    if let Some(p) = flag {
        return p.to_string();
    }
    if let Ok(p) = std::env::var("SOURCEDUMP_PYTHON") {
        if !p.is_empty() {
            return p;
        }
    }
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        if !venv.is_empty() {
            return format!("{venv}/bin/python");
        }
    }
    "python3".to_string()
}
