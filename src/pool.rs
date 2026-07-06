//! Process-pool backend: fan the work out across N Python/angr workers.
//!
//! Each worker is the embedded `worker.py` script run under the resolved
//! interpreter. Worker *i* of *N* handles `functions[i::N]` of the same
//! deterministically-sorted candidate list, so the union of all workers covers
//! the binary exactly once with no coordination between them.
//!
//! Worker stdout (newline-delimited JSON) is consumed as it arrives so the
//! caller (progress display or TUI) can react live; stderr is drained on a side
//! thread and surfaced only if a worker fails.

use crate::backend::{DecompileBackend, DecompileRequest, DecompileResult, Mode};
use crate::model::WorkerMessage;
use crate::progress::Progress;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::hash_map::RandomState;
use std::fs::{DirBuilder, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread;

/// The Python worker, baked into the binary so srcdump stays self-contained.
const WORKER_SOURCE: &str = include_str!("worker.py");

/// Tracks live worker PIDs so an interactive front end can terminate them on
/// quit instead of leaving orphaned Python processes decompiling in the
/// background.
#[derive(Default)]
pub struct Cancellation {
    cancelled: AtomicBool,
    pids: Mutex<Vec<u32>>,
}

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Record a freshly-spawned worker. If cancellation already happened, kill
    /// it immediately. The mutex serializes against [`Self::cancel`] so a pid
    /// can never slip through between the check and the push.
    fn register(&self, pid: u32) {
        let mut pids = self.pids.lock().unwrap();
        if self.cancelled.load(Ordering::Relaxed) {
            kill(pid);
        } else {
            pids.push(pid);
        }
    }

    /// Mark cancelled and SIGKILL every tracked worker.
    pub fn cancel(&self) {
        let mut pids = self.pids.lock().unwrap();
        self.cancelled.store(true, Ordering::Relaxed);
        for pid in pids.drain(..) {
            kill(pid);
        }
    }
}

#[cfg(unix)]
fn kill(pid: u32) {
    // Best effort; a pid that already exited just yields ESRCH.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill(pid: u32) {
    // No fork/signal model on Windows; terminate the process tree via taskkill.
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Random hex token drawn from `RandomState`, whose SipHash keys are seeded
/// from OS entropy — unpredictable temp names without a rand dependency.
fn random_token() -> String {
    fn draw() -> u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(0);
        hasher.finish()
    }
    format!("{:016x}{:016x}", draw(), draw())
}

/// Create a fresh private subdirectory under the system temp dir.
///
/// The name is random and `create_dir` refuses to reuse an existing path, so
/// we never adopt a directory another user prepared. On Unix it is created
/// mode 0o700 so no other user can write into or traverse it.
fn create_private_dir() -> Result<PathBuf> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for _ in 0..16 {
        let dir = std::env::temp_dir().join(format!("srcdump-{}", random_token()));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("creating worker dir {}", dir.display()))
            }
        }
    }
    bail!(
        "could not create a private worker dir under {}",
        std::env::temp_dir().display()
    );
}

pub struct ProcessPoolBackend {
    worker_dir: PathBuf,
    worker_script: PathBuf,
}

impl ProcessPoolBackend {
    /// Materialize the embedded worker script to a randomly-named file inside
    /// a private (user-only) temp subdirectory for this process.
    pub fn new() -> Result<Self> {
        let dir = create_private_dir()?;
        let path = dir.join(format!("worker-{}.py", random_token()));
        // Unlink anything already occupying the name, then create exclusively
        // (`create_new` never follows or truncates an existing file), so a
        // pre-planted file or symlink can neither survive nor be written
        // through.
        let _ = std::fs::remove_file(&path);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&path)
            .with_context(|| format!("creating worker script {}", path.display()))?;
        file.write_all(WORKER_SOURCE.as_bytes())
            .with_context(|| format!("writing worker script to {}", path.display()))?;
        Ok(Self {
            worker_dir: dir,
            worker_script: path,
        })
    }
}

impl Drop for ProcessPoolBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.worker_script);
        let _ = std::fs::remove_dir(&self.worker_dir);
    }
}

/// Result of running a single worker process to completion.
struct WorkerOutcome {
    index: usize,
    stderr_tail: String,
    status_ok: bool,
}

impl ProcessPoolBackend {
    fn build_command(&self, req: &DecompileRequest, mode: Mode, index: usize) -> Command {
        let mut cmd = Command::new(&req.python);
        cmd.arg(&self.worker_script);
        // The build worker reads the binary; decompile/list workers load
        // everything they need from the cached angrdb.
        if mode == Mode::Build {
            cmd.arg(&req.binary);
        }
        cmd.arg("--db")
            .arg(&req.db_path)
            .arg("--mode")
            .arg(mode.as_arg())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if mode != Mode::Build {
            cmd.arg("--index")
                .arg(index.to_string())
                .arg("--count")
                .arg(req.jobs.to_string());
            for f in &req.functions {
                cmd.arg("--function").arg(f);
            }
            if req.highlight {
                cmd.arg("--highlight");
            }
            // Worker 0 persists the catalog for both list and decompile runs.
            cmd.arg("--catalog-cache").arg(&req.catalog_cache);
            // The decompilation cache only applies to the decompile workers.
            if mode == Mode::Decompile {
                cmd.arg("--decomp-cache").arg(&req.decomp_cache);
                if req.force_decompile {
                    cmd.arg("--force-decompile");
                }
            }
        }
        if req.load_libs {
            cmd.arg("--load-libs");
        }
        if req.include_plt {
            cmd.arg("--include-plt");
        }
        cmd
    }

    /// Spawn one worker and stream its messages to `sink` as they arrive.
    ///
    /// stdout is parsed on the calling thread; stderr is drained on a side
    /// thread (so a noisy worker can never deadlock on a full pipe).
    fn run_worker(
        &self,
        req: &DecompileRequest,
        mode: Mode,
        index: usize,
        cancel: &Cancellation,
        mut sink: impl FnMut(WorkerMessage),
    ) -> Result<WorkerOutcome> {
        let mut child = self
            .build_command(req, mode, index)
            .spawn()
            .with_context(|| format!("spawning worker via `{}`", req.python.display()))?;
        cancel.register(child.id());

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let stderr_handle = thread::spawn(move || -> String {
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut buf);
            tail_lines(&buf, 20)
        });

        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<WorkerMessage>(&line) {
                sink(msg);
            }
        }

        let stderr_tail = stderr_handle
            .join()
            .map_err(|_| anyhow!("stderr thread panicked"))?;
        let status = child.wait().context("waiting on worker")?;

        Ok(WorkerOutcome {
            index,
            stderr_tail,
            status_ok: status.success(),
        })
    }

    /// Make sure the cached CFG angrdb exists, building it once if needed.
    /// Forwards the build worker's loading/CFG progress messages to `sink`.
    fn ensure_db(
        &self,
        req: &DecompileRequest,
        cancel: &Cancellation,
        sink: &mut impl FnMut(WorkerMessage),
    ) -> Result<()> {
        if !req.rebuild && req.db_path.exists() {
            return Ok(());
        }
        if let Some(parent) = req.db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }

        let mut fatal: Option<String> = None;
        let outcome = self.run_worker(req, Mode::Build, 0, cancel, |msg| {
            if let WorkerMessage::Fatal { message } = &msg {
                fatal = Some(message.clone());
            }
            sink(msg);
        })?;

        if cancel.is_cancelled() {
            return Ok(());
        }
        if !outcome.status_ok || !req.db_path.exists() {
            let detail = fatal
                .or_else(|| (!outcome.stderr_tail.is_empty()).then(|| outcome.stderr_tail.clone()))
                .unwrap_or_else(|| "build worker produced no angrdb".to_string());
            bail!("failed to build CFG cache: {detail}");
        }
        Ok(())
    }

    /// Run the full pipeline (ensure the CFG cache, then the decompile/list
    /// workers), delivering every worker message to `sink` as it arrives.
    ///
    /// `sink` is always invoked from a single thread. Build-phase failures
    /// return `Err`; per-worker crashes are surfaced as `Fatal` messages.
    pub fn run_streaming(
        &self,
        req: &DecompileRequest,
        cancel: &Cancellation,
        mut sink: impl FnMut(WorkerMessage),
    ) -> Result<()> {
        self.ensure_db(req, cancel, &mut sink)?;
        if cancel.is_cancelled() {
            return Ok(());
        }

        let (tx, rx) = mpsc::channel::<WorkerMessage>();
        let outcomes: Vec<Result<WorkerOutcome>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..req.jobs)
                .map(|index| {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        self.run_worker(req, req.mode, index, cancel, move |m| {
                            let _ = tx.send(m);
                        })
                    })
                })
                .collect();
            // Drop our own sender so `rx` closes once every worker has finished.
            drop(tx);

            for msg in &rx {
                sink(msg);
            }

            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("worker thread panicked")))
                })
                .collect()
        });

        for outcome in outcomes {
            let outcome = outcome?;
            // A worker killed by cancellation exits non-zero on purpose; don't
            // report that as a failure.
            if !outcome.status_ok && !cancel.is_cancelled() {
                let detail = if outcome.stderr_tail.is_empty() {
                    String::new()
                } else {
                    format!(":\n{}", outcome.stderr_tail)
                };
                sink(WorkerMessage::Fatal {
                    message: format!("worker {} exited non-zero{}", outcome.index, detail),
                });
            }
        }
        Ok(())
    }
}

impl DecompileBackend for ProcessPoolBackend {
    fn run(&self, req: &DecompileRequest, progress: &mut Progress) -> Result<DecompileResult> {
        let cancel = Cancellation::default();
        let mut result = DecompileResult::default();
        let mut fatals = Vec::new();
        let mut began = false;

        self.run_streaming(req, &cancel, |msg| {
            // The first non-build message marks the start of the work stage.
            if !began && !matches!(msg, WorkerMessage::Progress { .. }) {
                progress.begin_work();
                began = true;
            }
            match msg {
                WorkerMessage::Progress { stage, percent } => progress.build_stage(&stage, percent),
                WorkerMessage::Meta {
                    file_format,
                    angr_version,
                } => {
                    if result.file_format.is_none() {
                        result.file_format = Some(file_format);
                    }
                    if result.angr_version.is_none() {
                        result.angr_version = angr_version;
                    }
                }
                WorkerMessage::Catalog { .. } => {}
                WorkerMessage::Plan { count } => progress.add_total(count),
                WorkerMessage::Function(f) => {
                    result.functions.push(f);
                    progress.inc_done();
                }
                WorkerMessage::Error(e) => {
                    result.errors.push(e);
                    progress.inc_done();
                }
                WorkerMessage::Fatal { message } => fatals.push(message),
            }
        })?;

        // A fatal with no output means the binary/environment is broken the
        // same way for all workers — surface it rather than a partial dump.
        if result.functions.is_empty() && result.errors.is_empty() && !fatals.is_empty() {
            bail!("{}", fatals.join("\n"));
        }

        result.functions.sort_by_key(|f| f.addr);
        result.errors.sort_by_key(|e| e.addr);
        Ok(result)
    }
}

/// Return the last `n` non-empty-trimmed lines of `s`.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_script_lives_in_private_random_dir() {
        let backend = ProcessPoolBackend::new().unwrap();

        assert!(backend.worker_script.is_file());
        assert_eq!(backend.worker_script.parent(), Some(&*backend.worker_dir));
        assert_eq!(backend.worker_dir.parent(), Some(&*std::env::temp_dir()));
        assert_eq!(
            std::fs::read_to_string(&backend.worker_script).unwrap(),
            WORKER_SOURCE
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&backend.worker_dir)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "worker dir must be user-only");
            let file_mode = std::fs::metadata(&backend.worker_script)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o777, 0o600, "worker script must be user-only");
        }

        // Two backends must never collide on paths.
        let other = ProcessPoolBackend::new().unwrap();
        assert_ne!(backend.worker_dir, other.worker_dir);
        assert_ne!(backend.worker_script, other.worker_script);

        // Drop removes both the script and its private directory.
        let (dir, script) = (backend.worker_dir.clone(), backend.worker_script.clone());
        drop(backend);
        assert!(!script.exists());
        assert!(!dir.exists());
        drop(other);
    }
}
