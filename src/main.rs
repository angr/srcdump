//! srcdump — an objdump-like front end that prints decompiled code.
//!
//! The binary is a pure orchestrator: it shards decompilation across a pool of
//! Python/angr worker processes and merges their output. See `worker.py` for
//! the per-worker logic and `pool.rs` for the process management.

mod backend;
mod cli;
mod env;
mod highlight;
mod model;
mod output;
mod pool;
mod progress;
mod tui;

use anyhow::{Context, Result};
use backend::{DecompileBackend, DecompileRequest, Mode};
use clap::Parser;
use cli::{Cli, ColorMode, TuiMode};
use pool::ProcessPoolBackend;
use progress::Progress;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

fn main() {
    if let Err(err) = run() {
        eprintln!("srcdump: error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    let env_opts = env::EnvOptions {
        python: cli.python.clone(),
        venv: cli.venv.clone(),
        env_dir: cli.env_dir.clone(),
        env_archive: cli.env_archive.clone(),
        python_version: cli.python_version.clone(),
        refresh: cli.refresh_env,
        assume_yes: cli.yes,
    };

    // Standalone action: remove the managed env and exit (no file needed).
    if cli.remove_env {
        return env::remove(&env_opts);
    }

    // clap guarantees `file` is present unless --remove-env was given.
    let file = cli.file.clone().expect("file required unless --remove-env");
    if !file.exists() {
        anyhow::bail!("no such file: {}", file.display());
    }

    let python = env::resolve(&env_opts).context("resolving Python environment")?;
    let mode = if cli.list { Mode::List } else { Mode::Decompile };
    let db_path =
        cache_db_path(&file, cli.load_libs, cli.cache_dir.as_deref()).context("resolving CFG cache path")?;

    // The interactive TUI replaces the plain dump only for whole-binary
    // decompilation; --list and --slowest are explicit reports that always go
    // to stdout.
    let want_tui = cli.slowest.is_none()
        && !cli.list
        && match cli.tui {
            TuiMode::Always => true,
            TuiMode::Never => false,
            TuiMode::Auto => io::stdout().is_terminal(),
        };
    let dump_color = match cli.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => io::stdout().is_terminal(),
    };
    // The TUI always highlights; the dump only when it will print colored code.
    let will_dump_code = !want_tui && cli.slowest.is_none() && !cli.list;
    let highlight = want_tui || (will_dump_code && dump_color);

    let req = DecompileRequest {
        binary: file.clone(),
        functions: cli.functions.clone(),
        load_libs: cli.load_libs,
        include_plt: cli.include_plt,
        jobs: cli.job_count(),
        mode,
        python,
        db_path,
        rebuild: cli.rebuild,
        highlight,
    };

    let backend = ProcessPoolBackend::new().context("initializing worker pool")?;

    if want_tui {
        return tui::run(backend, req, file);
    }

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
        output::print_slowest(&mut out, &file, &result, n)?;
    } else {
        match mode {
            Mode::List => output::print_listing(&mut out, &file, &result)?,
            // `mode` is only ever List or Decompile here; Build is internal to the pool.
            Mode::Decompile | Mode::Build => {
                output::print_decompilation(&mut out, &file, &result, dump_color)?
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
fn cache_db_path(file: &Path, load_libs: bool, cache_dir: Option<&Path>) -> Result<PathBuf> {
    let digest = sha256_file(file).with_context(|| format!("hashing {}", file.display()))?;
    let filename = if load_libs {
        "db.load_libs.adb"
    } else {
        "db.adb"
    };
    Ok(cache_root(cache_dir).join(digest).join(filename))
}

/// Base directory for the cache: `--cache-dir`, else `$XDG_CACHE_HOME`, else
/// `$HOME/.cache`, else the system temp dir — each with a `srcdump` suffix.
fn cache_root(cache_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = cache_dir {
        return dir.to_path_buf();
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
