//! srcdump — an objdump-like front end that prints decompiled code.
//!
//! The binary is a pure orchestrator: it shards decompilation across a pool of
//! Python/angr worker processes and merges their output. See `worker.py` for
//! the per-worker logic and `pool.rs` for the process management.

mod backend;
mod cli;
mod model;
mod output;
mod pool;
mod progress;

use anyhow::{Context, Result};
use backend::{DecompileBackend, DecompileRequest, Mode};
use clap::Parser;
use cli::Cli;
use pool::ProcessPoolBackend;
use progress::Progress;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

fn main() {
    if let Err(err) = run() {
        eprintln!("srcdump: error: {err:#}");
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
    let db_path = cache_db_path(&cli).context("resolving CFG cache path")?;

    let req = DecompileRequest {
        binary: cli.file.clone(),
        functions: cli.functions.clone(),
        load_libs: cli.load_libs,
        include_plt: cli.include_plt,
        jobs: cli.job_count(),
        mode,
        python,
        db_path,
        rebuild: cli.rebuild,
    };

    let backend = ProcessPoolBackend::new().context("initializing worker pool")?;
    let mut progress = Progress::new(cli.progress, cli.list);
    let result = backend.run(&req, &mut progress)?;

    let verb = if cli.list { "Listed" } else { "Decompiled" };
    progress.finish(&format!(
        "{verb} {} function(s) in {}",
        result.functions.len(),
        progress.elapsed_str()
    ));

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if let Some(n) = cli.slowest {
        output::print_slowest(&mut out, &cli.file, &result, n)?;
    } else {
        match mode {
            Mode::List => output::print_listing(&mut out, &cli.file, &result)?,
            // `mode` is only ever List or Decompile here; Build is internal to the pool.
            Mode::Decompile | Mode::Build => {
                output::print_decompilation(&mut out, &cli.file, &result)?
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// Resolve the angrdb cache path for this binary:
/// `<cache-root>/<sha256>/db[.load_libs].adb`.
///
/// `auto_load_libs` changes the recovered CFG, so it is encoded in the filename
/// to keep the two variants from colliding.
fn cache_db_path(cli: &Cli) -> Result<PathBuf> {
    let digest = sha256_file(&cli.file)
        .with_context(|| format!("hashing {}", cli.file.display()))?;
    let filename = if cli.load_libs {
        "db.load_libs.adb"
    } else {
        "db.adb"
    };
    Ok(cache_root(cli).join(digest).join(filename))
}

/// Base directory for the cache: `--cache-dir`, else `$XDG_CACHE_HOME`, else
/// `$HOME/.cache`, else the system temp dir — each with a `srcdump` suffix.
fn cache_root(cli: &Cli) -> PathBuf {
    if let Some(dir) = &cli.cache_dir {
        return dir.clone();
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("srcdump");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".cache").join("srcdump");
    }
    std::env::temp_dir().join("srcdump")
}

/// Streaming SHA-256 of a file's contents, hex-encoded.
fn sha256_file(path: &std::path::Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Resolve the Python interpreter used to run workers.
///
/// Precedence: `--python` flag, then `$SRCDUMP_PYTHON`, then an active
/// virtualenv (`$VIRTUAL_ENV/bin/python`), then plain `python3` on PATH.
fn resolve_python(flag: Option<&str>) -> String {
    if let Some(p) = flag {
        return p.to_string();
    }
    if let Ok(p) = std::env::var("SRCDUMP_PYTHON") {
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
