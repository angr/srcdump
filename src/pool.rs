//! Process-pool backend: fan the work out across N Python/angr workers.
//!
//! Each worker is the embedded `worker.py` script run under the resolved
//! interpreter. Worker *i* of *N* handles `functions[i::N]` of the same
//! deterministically-sorted candidate list, so the union of all workers covers
//! the binary exactly once with no coordination between them.

use crate::backend::{DecompileBackend, DecompileRequest, DecompileResult};
use crate::model::WorkerMessage;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

/// The Python worker, baked into the binary so sourcedump stays self-contained.
const WORKER_SOURCE: &str = include_str!("worker.py");

pub struct ProcessPoolBackend {
    worker_script: PathBuf,
}

impl ProcessPoolBackend {
    /// Materialize the embedded worker script to a temp file for this process.
    pub fn new() -> Result<Self> {
        let mut path = std::env::temp_dir();
        path.push(format!("sourcedump_worker_{}.py", std::process::id()));
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

/// Output gathered from a single worker process.
struct WorkerOutput {
    index: usize,
    messages: Vec<WorkerMessage>,
    stderr_tail: String,
    status_ok: bool,
}

impl ProcessPoolBackend {
    fn spawn_worker(&self, req: &DecompileRequest, index: usize) -> Result<WorkerOutput> {
        let mut cmd = Command::new(&req.python);
        cmd.arg(&self.worker_script)
            .arg(&req.binary)
            .arg("--index")
            .arg(index.to_string())
            .arg("--count")
            .arg(req.jobs.to_string())
            .arg("--mode")
            .arg(req.mode.as_arg())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if req.load_libs {
            cmd.arg("--load-libs");
        }
        if req.include_plt {
            cmd.arg("--include-plt");
        }
        for f in &req.functions {
            cmd.arg("--function").arg(f);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning worker via `{}`", req.python))?;

        // Drain stdout and stderr on separate threads so a worker that produces
        // a lot of output can never deadlock on a full pipe.
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let stdout_handle = thread::spawn(move || -> Vec<WorkerMessage> {
            let reader = BufReader::new(stdout);
            let mut msgs = Vec::new();
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<WorkerMessage>(&line) {
                    Ok(msg) => msgs.push(msg),
                    Err(_) => { /* ignore stray non-JSON output */ }
                }
            }
            msgs
        });

        let stderr_handle = thread::spawn(move || -> String {
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut buf);
            // Keep only the tail to surface on failure without flooding.
            tail_lines(&buf, 20)
        });

        let messages = stdout_handle.join().map_err(|_| anyhow!("stdout thread panicked"))?;
        let stderr_tail = stderr_handle.join().map_err(|_| anyhow!("stderr thread panicked"))?;
        let status = child.wait().context("waiting on worker")?;

        Ok(WorkerOutput {
            index,
            messages,
            stderr_tail,
            status_ok: status.success(),
        })
    }
}

impl DecompileBackend for ProcessPoolBackend {
    fn run(&self, req: &DecompileRequest) -> Result<DecompileResult> {
        // Spawn every worker on its own thread; each thread owns one subprocess
        // end-to-end (spawn, drain, wait).
        let outputs: Vec<Result<WorkerOutput>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..req.jobs)
                .map(|index| scope.spawn(move || self.spawn_worker(req, index)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("worker thread panicked"))))
                .collect()
        });

        let mut result = DecompileResult::default();
        let mut fatals = Vec::new();
        for out in outputs {
            let out = out?;
            for msg in out.messages {
                match msg {
                    WorkerMessage::Function(f) => result.functions.push(f),
                    WorkerMessage::Error(e) => result.errors.push(e),
                    WorkerMessage::Fatal { message } => {
                        fatals.push(format!("worker {}: {}", out.index, message))
                    }
                    WorkerMessage::Meta { file_format } => {
                        if result.file_format.is_none() {
                            result.file_format = Some(file_format);
                        }
                    }
                }
            }
            if !out.status_ok && fatals.is_empty() {
                fatals.push(format!(
                    "worker {} exited non-zero{}",
                    out.index,
                    if out.stderr_tail.is_empty() {
                        String::new()
                    } else {
                        format!(":\n{}", out.stderr_tail)
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
