//! Backend abstraction for turning a binary into decompiled functions.
//!
//! Only one backend exists today ([`crate::pool::ProcessPoolBackend`]), but the
//! trait keeps the orchestration code decoupled from *how* decompilation
//! happens, leaving room for e.g. an in-process or CFG-cached backend later.

use crate::model::{FunctionError, FunctionInfo};
use crate::progress::Progress;
use anyhow::Result;
use std::path::PathBuf;

/// What the workers should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Recover the CFG and persist it to an angrdb (one-shot, internal).
    Build,
    /// Fully decompile each function.
    Decompile,
    /// Only enumerate functions (no decompilation).
    List,
}

impl Mode {
    pub fn as_arg(self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::Decompile => "decompile",
            Mode::List => "list",
        }
    }
}

/// A complete decompilation job.
#[derive(Debug, Clone)]
pub struct DecompileRequest {
    pub binary: PathBuf,
    /// Name/address filters (empty = whole binary).
    pub functions: Vec<String>,
    pub load_libs: bool,
    pub include_plt: bool,
    pub jobs: usize,
    pub mode: Mode,
    /// Python interpreter to launch workers with.
    pub python: PathBuf,
    /// Where the cached CFG angrdb lives.
    pub db_path: PathBuf,
    /// Rebuild the cached angrdb even if it already exists.
    pub rebuild: bool,
    /// Ask workers to emit semantic highlight spans with each function.
    pub highlight: bool,
    /// Directory holding cached per-function decompilation results.
    pub decomp_cache: PathBuf,
    /// File where worker 0 persists the full function catalog (full runs only).
    pub catalog_cache: PathBuf,
    /// Re-decompile and overwrite the cache instead of reading it.
    pub force_decompile: bool,
}

/// Aggregated output of a job, already merged across all workers.
#[derive(Debug, Default)]
pub struct DecompileResult {
    /// objdump/BFD-style file format, e.g. `elf64-x86-64`.
    pub file_format: Option<String>,
    /// Functions, sorted by address.
    pub functions: Vec<FunctionInfo>,
    /// Per-function decompilation failures, sorted by address.
    pub errors: Vec<FunctionError>,
}

pub trait DecompileBackend {
    fn run(&self, req: &DecompileRequest, progress: &mut Progress) -> Result<DecompileResult>;
}
