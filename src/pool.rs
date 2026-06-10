//! Process-pool backend: fan the work out across N Python/angr workers.
//!
//! Each worker is the embedded `worker.py` script run under the resolved
//! interpreter. Worker *i* of *N* handles `functions[i::N]` of the same
//! deterministically-sorted candidate list, so the union of all workers covers
//! the binary exactly once with no coordination between them.
//!
//! Worker stdout (newline-delimited JSON) is consumed as it arrives so the
//! progress display can update live; stderr is drained on a side thread and
//! surfaced only if a worker fails.

use crate::backend::{DecompileBackend, DecompileRequest, DecompileResult, Mode};
use crate::model::WorkerMessage;
use crate::progress::Progress;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// The Python worker, baked into the binary so srcdump stays self-contained.
const WORKER_SOURCE: &str = include_str!("worker.py");

pub struct ProcessPoolBackend {
    worker_script: PathBuf,
}

impl ProcessPoolBackend {
    /// Materialize the embedded worker script to a temp file for this process.
    pub fn new() -> Result<Self> {
        let mut path = std::env::temp_dir();
        path.push(format!("srcdump_worker_{}.py", std::process::id()));
        std::fs::write(&path, WORKER_SOURCE)
            .with_context(|| format!("writing worker script to {}", path.display()))?;
        Ok(Self {
            worker_script: path,
        })
    }
}

impl Drop for ProcessPoolBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.worker_script);
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
        mut sink: impl FnMut(WorkerMessage),
    ) -> Result<WorkerOutcome> {
        let mut child = self
            .build_command(req, mode, index)
            .spawn()
            .with_context(|| format!("spawning worker via `{}`", req.python))?;

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
    /// Streams the build worker's loading/CFG progress to `progress`.
    fn ensure_db(&self, req: &DecompileRequest, progress: &mut Progress) -> Result<()> {
        if !req.rebuild && req.db_path.exists() {
            return Ok(());
        }
        if let Some(parent) = req.db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }

        let mut fatal: Option<String> = None;
        let outcome = self.run_worker(req, Mode::Build, 0, |msg| match msg {
            WorkerMessage::Progress { stage, percent } => progress.build_stage(&stage, percent),
            WorkerMessage::Fatal { message } => fatal = Some(message),
            _ => {}
        })?;

        if !outcome.status_ok || !req.db_path.exists() {
            let detail = fatal
                .or_else(|| (!outcome.stderr_tail.is_empty()).then(|| outcome.stderr_tail.clone()))
                .unwrap_or_else(|| "build worker produced no angrdb".to_string());
            bail!("failed to build CFG cache: {detail}");
        }
        Ok(())
    }
}

impl DecompileBackend for ProcessPoolBackend {
    fn run(&self, req: &DecompileRequest, progress: &mut Progress) -> Result<DecompileResult> {
        // Recover the CFG once into the shared angrdb, then fan out workers
        // that each load it instead of rebuilding the CFG.
        self.ensure_db(req, progress)?;
        progress.begin_work();

        let mut result = DecompileResult::default();
        let mut fatals = Vec::new();

        // Each worker streams its messages over the channel; this (main) thread
        // consumes them, updating progress and accumulating results live.
        let (tx, rx) = mpsc::channel::<(usize, WorkerMessage)>();
        let outcomes: Vec<Result<WorkerOutcome>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..req.jobs)
                .map(|index| {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        self.run_worker(req, req.mode, index, |m| {
                            let _ = tx.send((index, m));
                        })
                    })
                })
                .collect();
            // Drop our own sender so `rx` closes once every worker has finished.
            drop(tx);

            for (worker_index, msg) in &rx {
                match msg {
                    WorkerMessage::Function(f) => {
                        result.functions.push(f);
                        progress.inc_done();
                    }
                    WorkerMessage::Error(e) => {
                        result.errors.push(e);
                        progress.inc_done();
                    }
                    WorkerMessage::Plan { count } => progress.add_total(count),
                    WorkerMessage::Meta { file_format } => {
                        if result.file_format.is_none() {
                            result.file_format = Some(file_format);
                        }
                    }
                    WorkerMessage::Fatal { message } => {
                        fatals.push(format!("worker {worker_index}: {message}"))
                    }
                    WorkerMessage::Progress { .. } => {}
                }
            }

            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("worker thread panicked"))))
                .collect()
        });

        for outcome in outcomes {
            let outcome = outcome?;
            if !outcome.status_ok && fatals.is_empty() {
                fatals.push(format!(
                    "worker {} exited non-zero{}",
                    outcome.index,
                    if outcome.stderr_tail.is_empty() {
                        String::new()
                    } else {
                        format!(":\n{}", outcome.stderr_tail)
                    }
                ));
            }
        }

        // A fatal in any worker means the binary/environment is broken the same
        // way for all of them — surface it rather than printing a partial dump.
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
