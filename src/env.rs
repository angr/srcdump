//! Resolving (and, by default, provisioning) the Python interpreter the workers
//! run under.
//!
//! Resolution order:
//! 1. `--python` / `$SRCDUMP_PYTHON` — an exact interpreter, used as-is.
//! 2. `--venv DIR` — an existing virtualenv with angr, used as-is.
//! 3. *(default)* a srcdump-managed environment. If a `uv_angr.zip` sits next
//!    to the binary (or `--env-archive` is given) it is extracted into the
//!    managed location; otherwise `uv` installs the latest angr from PyPI,
//!    downloading `uv` itself first if it isn't already available.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Inputs that steer environment resolution (mirrors the relevant CLI flags).
pub struct EnvOptions {
    pub python: Option<PathBuf>,
    pub venv: Option<PathBuf>,
    pub env_dir: Option<PathBuf>,
    pub env_archive: Option<PathBuf>,
    pub python_version: String,
    pub refresh: bool,
    /// Skip confirmation prompts (`--yes`).
    pub assume_yes: bool,
}

/// Resolve the interpreter, provisioning the managed env if necessary.
pub fn resolve(opts: &EnvOptions) -> Result<PathBuf> {
    if let Some(p) = &opts.python {
        return Ok(p.clone());
    }
    if let Some(p) = std::env::var_os("SRCDUMP_PYTHON").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    if let Some(dir) = &opts.venv {
        let py = dir.join("bin").join("python");
        if !py.is_file() {
            bail!(
                "--venv {}: no interpreter at {} (is it a venv with angr installed?)",
                dir.display(),
                py.display()
            );
        }
        return Ok(py);
    }
    ensure_managed(opts)
}

fn status(msg: &str) {
    eprintln!("srcdump: {msg}");
}

/// The exact command that removes whatever managed env we're about to create.
fn remove_hint(opts: &EnvOptions, env_dir: &Path) -> String {
    let custom = opts.env_dir.is_some() || std::env::var_os("SRCDUMP_ENV_DIR").is_some();
    if custom {
        format!("srcdump --remove-env --env-dir {}", env_dir.display())
    } else {
        "srcdump --remove-env".to_string()
    }
}

/// First-run notice shown before uv provisioning, telling the user what srcdump
/// is about to download/install and how to undo it.
fn print_uv_notice(opts: &EnvOptions, env_dir: &Path, data_dir: &Path) {
    let needs_uv = !uv_on_path() && !data_dir.join("bin").join("uv").is_file();
    eprintln!();
    eprintln!("srcdump: setting up the angr environment (first run). This will:");
    if needs_uv {
        eprintln!("    • download uv (the Python installer)");
    }
    eprintln!(
        "    • create a Python {} virtualenv at {}",
        opts.python_version,
        env_dir.display()
    );
    eprintln!("    • install the latest angr from PyPI (downloads a few hundred MB)");
    eprintln!("  Remove it later with:  {}", remove_hint(opts, env_dir));
    eprintln!();
}

// ---------------------------------------------------------------------------
// Managed environment
// ---------------------------------------------------------------------------

/// Location of the managed env: `--env-dir`, else `$SRCDUMP_ENV_DIR`, else the
/// default under the data dir.
fn resolve_env_dir(opts: &EnvOptions) -> PathBuf {
    opts.env_dir
        .clone()
        .or_else(|| std::env::var_os("SRCDUMP_ENV_DIR").map(PathBuf::from))
        .unwrap_or_else(|| data_dir().join("env"))
}

/// Delete the managed environment. Used by `--remove-env`. Prompts for
/// confirmation unless `--yes` is set.
pub fn remove(opts: &EnvOptions) -> Result<()> {
    let env_dir = resolve_env_dir(opts);
    if !env_dir.is_dir() {
        status(&format!(
            "no managed environment at {} (nothing to remove)",
            env_dir.display()
        ));
        return Ok(());
    }
    if !opts.assume_yes
        && !confirm(
            &format!("Remove managed environment at {}?", env_dir.display()),
            false,
        )?
    {
        status("aborted; nothing removed");
        return Ok(());
    }
    std::fs::remove_dir_all(&env_dir).with_context(|| format!("removing {}", env_dir.display()))?;
    status(&format!(
        "removed managed environment at {}",
        env_dir.display()
    ));
    Ok(())
}

/// Ask a yes/no question on the terminal; empty input takes `default_yes`.
/// Without a TTY we can't ask, so return `false` and point at `--yes`.
fn confirm(question: &str, default_yes: bool) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("srcdump: {question} (pass --yes to confirm non-interactively)");
        return Ok(false);
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprint!("{question} {hint} ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let ans = line.trim().to_ascii_lowercase();
    if ans.is_empty() {
        return Ok(default_yes);
    }
    Ok(ans == "y" || ans == "yes")
}

fn ensure_managed(opts: &EnvOptions) -> Result<PathBuf> {
    let data_dir = data_dir();
    let env_dir = resolve_env_dir(opts);
    let python = env_dir.join("bin").join("python");

    let archive = resolve_archive(opts)?;
    let token = match &archive {
        Some(a) => archive_token(a)?,
        None => "uv".to_string(),
    };
    let marker = env_dir.join(".srcdump-ready");

    // Fast path: already provisioned for this exact source.
    if !opts.refresh && python.is_file() && read_marker(&marker).as_deref() == Some(token.as_str())
    {
        return Ok(python);
    }

    // Serialize provisioning across concurrent srcdump invocations.
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let _lock = Flock::acquire(&data_dir.join("setup.lock"))?;

    // Re-check now that we hold the lock (another process may have finished).
    if !opts.refresh && python.is_file() && read_marker(&marker).as_deref() == Some(token.as_str())
    {
        return Ok(python);
    }

    match &archive {
        Some(a) => {
            status(&format!(
                "extracting bundled angr environment from {} into {}",
                a.display(),
                env_dir.display()
            ));
            eprintln!("  Remove it later with:  {}", remove_hint(opts, &env_dir));
            provision_from_archive(a, &env_dir)?;
        }
        None => {
            print_uv_notice(opts, &env_dir, &data_dir);
            if !opts.assume_yes && !confirm("Set up the angr environment now?", true)? {
                bail!("angr environment setup declined; re-run with --yes to skip this prompt");
            }
            provision_with_uv(opts, &env_dir, &data_dir)?;
        }
    }

    if !python.is_file() {
        bail!("environment setup did not produce {}", python.display());
    }
    write_marker(&marker, &token)?;
    status("environment ready");
    Ok(python)
}

/// Pick the env archive: explicit `--env-archive`, else an auto-detected
/// `uv_angr.zip` next to the binary, else none (→ use uv).
fn resolve_archive(opts: &EnvOptions) -> Result<Option<PathBuf>> {
    if let Some(a) = &opts.env_archive {
        if a.is_file() {
            return Ok(Some(a.clone()));
        }
        bail!("--env-archive: file not found: {}", a.display());
    }
    if let Some(dir) = exe_dir() {
        let candidate = dir.join("uv_angr.zip");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Provisioning via uv
// ---------------------------------------------------------------------------

fn provision_with_uv(opts: &EnvOptions, env_dir: &Path, data_dir: &Path) -> Result<()> {
    let uv = ensure_uv(data_dir)?;
    let python = env_dir.join("bin").join("python");

    if !python.is_file() {
        run(
            Command::new(&uv)
                .arg("venv")
                .arg("--python")
                .arg(&opts.python_version)
                .arg(env_dir),
            "uv venv",
        )?;
    }

    let mut cmd = Command::new(&uv);
    cmd.arg("pip").arg("install").arg("--python").arg(&python);
    if opts.refresh {
        cmd.arg("--upgrade");
    }
    // sqlalchemy backs angr's AngrDB, which srcdump uses for the CFG cache.
    cmd.arg("angr").arg("sqlalchemy");
    run(&mut cmd, "uv pip install angr")?;
    Ok(())
}

/// Find a usable `uv`: on PATH, else previously downloaded, else download it.
fn ensure_uv(data_dir: &Path) -> Result<PathBuf> {
    if uv_on_path() {
        return Ok(PathBuf::from("uv"));
    }
    let local = data_dir.join("bin").join("uv");
    if local.is_file() {
        return Ok(local);
    }
    download_uv(&data_dir.join("bin"))
}

fn uv_on_path() -> bool {
    Command::new("uv")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn download_uv(bin_dir: &Path) -> Result<PathBuf> {
    let triple = uv_target_triple()?;
    let url =
        format!("https://github.com/astral-sh/uv/releases/latest/download/uv-{triple}.tar.gz");
    status(&format!("downloading uv ({triple})…"));
    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;

    let resp = ureq::get(&url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let reader = resp.into_body().into_reader();
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);

    let dest = bin_dir.join("uv");
    for entry in archive.entries().context("reading uv archive")? {
        let mut entry = entry?;
        let is_uv = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n == "uv"))
            .unwrap_or(false);
        if is_uv {
            let mut out = std::fs::File::create(&dest)
                .with_context(|| format!("writing {}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)?;
            set_executable(&dest)?;
            return Ok(dest);
        }
    }
    bail!("uv binary not found inside {url}");
}

fn uv_target_triple() -> Result<&'static str> {
    use std::env::consts::{ARCH, OS};
    Ok(match (OS, ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => bail!("no prebuilt uv for {os}/{arch}; install uv manually and re-run"),
    })
}

// ---------------------------------------------------------------------------
// Provisioning from a prebuilt zip archive
// ---------------------------------------------------------------------------

fn provision_from_archive(archive: &Path, env_dir: &Path) -> Result<()> {
    let mut tmp = env_dir.as_os_str().to_owned();
    tmp.push(".extract.tmp");
    let tmp = PathBuf::from(tmp);

    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    extract_zip(archive, &tmp)?;

    let root = find_venv_root(&tmp).ok_or_else(|| {
        anyhow!(
            "{} does not contain a venv (no bin/python found)",
            archive.display()
        )
    })?;

    let _ = std::fs::remove_dir_all(env_dir);
    if let Some(parent) = env_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&root, env_dir)
        .with_context(|| format!("installing extracted env into {}", env_dir.display()))?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// A venv root is a directory containing `bin/python`, either the extraction
/// dir itself or a single top-level subdirectory of it.
fn find_venv_root(dir: &Path) -> Option<PathBuf> {
    if dir.join("bin").join("python").is_file() {
        return Some(dir.to_path_buf());
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join("bin").join("python").is_file() {
            return Some(p);
        }
    }
    None
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", zip_path.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        // `enclosed_name` rejects absolute paths and `..` (zip-slip safe).
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mode = entry.unix_mode();
        if is_symlink(mode) {
            let mut target = String::new();
            entry.read_to_string(&mut target)?;
            let _ = std::fs::remove_file(&out);
            symlink(&target, &out)?;
            continue;
        }

        let mut outfile =
            std::fs::File::create(&out).with_context(|| format!("writing {}", out.display()))?;
        std::io::copy(&mut entry, &mut outfile)?;
        apply_mode(&out, mode)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("running {what}"))?;
    if !status.success() {
        bail!("{what} failed (exit {:?})", status.code());
    }
    Ok(())
}

fn data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(x).join("srcdump");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h)
            .join(".local")
            .join("share")
            .join("srcdump");
    }
    std::env::temp_dir().join("srcdump")
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

fn read_marker(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_marker(p: &Path, token: &str) -> Result<()> {
    std::fs::write(p, token).with_context(|| format!("writing {}", p.display()))
}

fn archive_token(a: &Path) -> Result<String> {
    let meta = std::fs::metadata(a)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("archive:{}:{}", meta.len(), mtime))
}

#[cfg(unix)]
fn is_symlink(mode: Option<u32>) -> bool {
    matches!(mode, Some(m) if m & 0o170000 == 0o120000)
}
#[cfg(not(unix))]
fn is_symlink(_mode: Option<u32>) -> bool {
    false
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {}", link.display()))
}
#[cfg(not(unix))]
fn symlink(_target: &str, _link: &Path) -> Result<()> {
    bail!("symlinks in archive are not supported on this platform")
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(m) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Advisory cross-process lock held for the duration of provisioning.
struct Flock {
    _file: std::fs::File,
}

#[cfg(unix)]
impl Flock {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            bail!(
                "locking {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { _file: file })
    }
}

#[cfg(not(unix))]
impl Flock {
    fn acquire(_path: &Path) -> Result<Self> {
        // Best-effort: no locking on non-unix.
        Ok(Self {
            _file: std::fs::File::open(std::env::temp_dir()).unwrap(),
        })
    }
}
