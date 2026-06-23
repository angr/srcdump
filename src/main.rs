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
mod sysmem;
mod tui;

use anyhow::{Context, Result};
use backend::{DecompileBackend, DecompileRequest, DecompileResult, Mode};
use clap::Parser;
use cli::{Cli, ColorMode, TuiMode};
use model::{CatalogCache, DecompCache, FunctionInfo};
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
    let mode = if cli.list {
        Mode::List
    } else {
        Mode::Decompile
    };

    // CFG db and decompilation cache live side by side under the binary's hash;
    // `.load_libs` distinguishes the auto_load_libs variant.
    let cache_base = cache_base(&file, cli.cache_dir.as_deref()).context("resolving cache path")?;
    let suffix = if cli.load_libs { ".load_libs" } else { "" };
    let db_path = cache_base.join(format!("db{suffix}.adb"));
    let decomp_cache = cache_base.join(format!("decomp{suffix}"));
    let plt = if cli.include_plt { ".plt" } else { "" };
    let catalog_cache = cache_base.join(format!("catalog{suffix}{plt}.json"));
    if cli.rebuild {
        let _ = std::fs::remove_dir_all(&decomp_cache);
        let _ = std::fs::remove_file(cache_base.join(format!("catalog{suffix}.json")));
        let _ = std::fs::remove_file(cache_base.join(format!("catalog{suffix}.plt.json")));
    }
    let force_decompile = cli.redecompile || cli.rebuild;

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

    // Warm path: if the catalog (and, for decompilation, every selected
    // function) is already cached, build the result in Rust and skip Python.
    if !force_decompile {
        if let Some(result) = try_cached(&catalog_cache, &decomp_cache, &cli.functions, mode)? {
            if want_tui {
                return tui::run_cached(result, file);
            }
            return print_result(&result, cli.slowest, mode, dump_color, &file);
        }
    }

    // Each angr worker can use a lot of RAM, so cap the worker count to what
    // available memory can support.
    let per_worker = ram_per_worker();
    let jobs = budget_workers(cli.job_count(), sysmem::available_bytes(), per_worker);
    if jobs < cli.job_count() {
        eprintln!(
            "srcdump: limiting to {jobs} worker(s) — {} available, budgeting {} each",
            sysmem::available_bytes()
                .map(sysmem::human_bytes)
                .unwrap_or_else(|| "?".into()),
            sysmem::human_bytes(per_worker),
        );
    }

    let req = DecompileRequest {
        binary: file.clone(),
        functions: cli.functions.clone(),
        load_libs: cli.load_libs,
        include_plt: cli.include_plt,
        jobs,
        mode,
        python,
        db_path,
        rebuild: cli.rebuild,
        highlight,
        decomp_cache,
        catalog_cache,
        force_decompile,
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

    print_result(&result, cli.slowest, mode, dump_color, &file)
}

/// Render a finished result to stdout (list / slowest / decompilation).
fn print_result(
    result: &DecompileResult,
    slowest: Option<usize>,
    mode: Mode,
    dump_color: bool,
    file: &Path,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if let Some(n) = slowest {
        output::print_slowest(&mut out, file, result, n)?;
    } else {
        match mode {
            Mode::List => output::print_listing(&mut out, file, result)?,
            // `mode` is only ever List or Decompile here; Build is internal to the pool.
            Mode::Decompile | Mode::Build => {
                output::print_decompilation(&mut out, file, result, dump_color)?
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// Try to satisfy the request entirely from cache, returning `None` (→ run the
/// Python workers) if the catalog or any needed decompilation is missing.
fn try_cached(
    catalog_cache: &Path,
    decomp_cache: &Path,
    filters: &[String],
    mode: Mode,
) -> Result<Option<DecompileResult>> {
    let Ok(bytes) = std::fs::read(catalog_cache) else {
        return Ok(None);
    };
    let Ok(catalog) = serde_json::from_slice::<CatalogCache>(&bytes) else {
        return Ok(None); // corrupt cache → just re-run
    };

    let mut selected: Vec<FunctionInfo> = catalog
        .functions
        .into_iter()
        .filter(|f| matches_filters(f, filters))
        .collect();
    selected.sort_by_key(|f| f.addr);

    // On a warm run no angr runs, so the catalog's version is the reference we
    // flag stale decompilations against.
    let reference = catalog.angr_version.clone();
    let mut result = DecompileResult {
        file_format: catalog.file_format,
        angr_version: (!reference.is_empty()).then(|| reference.clone()),
        ..Default::default()
    };

    if mode == Mode::List {
        result.functions = selected;
        return Ok(Some(result));
    }

    // Decompilation: every selected function must be in the cache, else bail.
    for mut func in selected {
        let path = decomp_cache.join(format!("0x{:x}.json", func.addr));
        let cached = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<DecompCache>(&b).ok());
        let Some(dc) = cached else {
            return Ok(None);
        };
        // Flag a decompilation produced by a different angr version.
        func.stale_version = match &dc.angr_version {
            Some(v) if *v != reference => Some(v.clone()),
            _ => None,
        };
        func.text = Some(dc.text);
        func.spans = dc.spans;
        func.decompile_ms = dc.decompile_ms;
        result.functions.push(func);
    }
    Ok(Some(result))
}

/// Whether `func` passes the `-f` name/address filters (empty = everything).
fn matches_filters(func: &FunctionInfo, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters
        .iter()
        .any(|raw| raw == &func.name || parse_int(raw).is_some_and(|addr| addr == func.addr))
}

/// Parse a `-f` address filter: `0x`-hex or decimal.
fn parse_int(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// RAM budgeted per angr worker by default.
const RAM_PER_WORKER: u64 = 3 * 1024 * 1024 * 1024;
/// Smaller budget on CI, where test binaries are tiny but runners are small —
/// so the cap doesn't needlessly throttle parallelism.
const CI_RAM_PER_WORKER: u64 = 1024 * 1024 * 1024;

/// The per-worker RAM budget: `$SRCDUMP_WORKER_RAM_MB` if set, else a smaller
/// value under CI, else the default.
fn ram_per_worker() -> u64 {
    if let Some(mb) = std::env::var("SRCDUMP_WORKER_RAM_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return mb * 1024 * 1024;
    }
    if running_on_ci() {
        return CI_RAM_PER_WORKER;
    }
    RAM_PER_WORKER
}

/// Whether we're running under a CI system (e.g. GitHub Actions sets `CI=true`).
fn running_on_ci() -> bool {
    std::env::var("CI")
        .map(|v| !matches!(v.as_str(), "" | "0" | "false" | "False" | "FALSE"))
        .unwrap_or(false)
}

/// Cap the worker count so each worker has roughly `per_worker` bytes of
/// headroom. `available` is available RAM in bytes, or `None` when it can't be
/// determined (then `requested` is left unchanged). Always allows at least one
/// worker.
fn budget_workers(requested: usize, available: Option<u64>, per_worker: u64) -> usize {
    match available {
        Some(bytes) => {
            let by_ram = (bytes / per_worker.max(1)).max(1) as usize;
            requested.min(by_ram)
        }
        None => requested,
    }
}

/// Per-binary cache directory: `<cache-root>/<sha256>`. Holds the CFG angrdb and
/// the decompilation cache (the `.load_libs` variant is encoded in their names).
fn cache_base(file: &Path, cache_dir: Option<&Path>) -> Result<PathBuf> {
    let digest = sha256_file(file).with_context(|| format!("hashing {}", file.display()))?;
    Ok(cache_root(cache_dir).join(digest))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique scratch dir for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("srcdump_test_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("decomp")).unwrap();
        dir
    }

    #[test]
    fn warm_path_flags_only_stale_functions() {
        let dir = scratch("stale");
        let decomp = dir.join("decomp");
        fs::write(
            dir.join("catalog.json"),
            r#"{"angr_version":"9.2.221","file_format":"elf64-x86-64",
                "functions":[{"addr":4096,"name":"main"},{"addr":8192,"name":"foo"}]}"#,
        )
        .unwrap();
        // main matches the catalog version; foo was produced by an older angr.
        fs::write(
            decomp.join("0x1000.json"),
            r#"{"text":"int main(){}","angr_version":"9.2.221"}"#,
        )
        .unwrap();
        fs::write(
            decomp.join("0x2000.json"),
            r#"{"text":"int foo(){}","angr_version":"9.0.0"}"#,
        )
        .unwrap();

        let res = try_cached(&dir.join("catalog.json"), &decomp, &[], Mode::Decompile)
            .unwrap()
            .expect("all functions cached -> warm hit");

        assert_eq!(res.angr_version.as_deref(), Some("9.2.221"));
        let main = res.functions.iter().find(|f| f.addr == 0x1000).unwrap();
        let foo = res.functions.iter().find(|f| f.addr == 0x2000).unwrap();
        assert_eq!(
            main.stale_version, None,
            "current-version function not flagged"
        );
        assert_eq!(
            foo.stale_version.as_deref(),
            Some("9.0.0"),
            "older-version function flagged"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn warm_path_bails_when_a_decompilation_is_missing() {
        let dir = scratch("miss");
        let decomp = dir.join("decomp");
        fs::write(
            dir.join("catalog.json"),
            r#"{"angr_version":"9.2.221","functions":[{"addr":4096,"name":"main"}]}"#,
        )
        .unwrap();
        // No decomp/0x1000.json -> can't serve from cache, must fall back to workers.
        let res = try_cached(&dir.join("catalog.json"), &decomp, &[], Mode::Decompile).unwrap();
        assert!(res.is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_budget_caps_by_available_ram() {
        let gib = 1024 * 1024 * 1024;
        // 64 GiB / 3 GiB = 21 -> capped from 32.
        assert_eq!(budget_workers(32, Some(64 * gib), 3 * gib), 21);
        // Requested below the RAM budget is left alone.
        assert_eq!(budget_workers(8, Some(64 * gib), 3 * gib), 8);
        // Under one budget unit still allows one worker.
        assert_eq!(budget_workers(32, Some(2 * gib), 3 * gib), 1);
        assert_eq!(budget_workers(32, Some(0), 3 * gib), 1);
        // A different (e.g. CI) budget changes the cap.
        assert_eq!(budget_workers(32, Some(8 * gib), gib), 8);
        // Unknown available memory -> no cap.
        assert_eq!(budget_workers(32, None, 3 * gib), 32);
    }

    fn func(name: &str, addr: u64) -> FunctionInfo {
        FunctionInfo {
            addr,
            name: name.into(),
            size: 0,
            is_plt: false,
            section: None,
            text: None,
            decompile_ms: None,
            spans: Vec::new(),
        }
    }

    #[test]
    fn parse_int_hex_and_decimal() {
        assert_eq!(parse_int("0x10"), Some(16));
        assert_eq!(parse_int("0X10"), Some(16));
        assert_eq!(parse_int("16"), Some(16));
        assert_eq!(parse_int("main"), None);
    }

    #[test]
    fn filters_match_name_and_address() {
        let f = func("main", 0x401000);
        assert!(matches_filters(&f, &[]));
        assert!(matches_filters(&f, &["main".into()]));
        assert!(matches_filters(&f, &["0x401000".into()]));
        assert!(matches_filters(&f, &["4198400".into()]));
        assert!(!matches_filters(&f, &["other".into()]));
    }
}
