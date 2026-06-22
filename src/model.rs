//! Shared data types exchanged between the Rust orchestrator and the Python
//! worker processes. The worker emits one JSON object per line; each line
//! deserializes into a [`WorkerMessage`].

use serde::{Deserialize, Serialize};

/// A single line emitted by a worker on stdout.
///
/// The worker tags every message with a `kind` so the orchestrator can route
/// it without ambiguity. Functions that decompile successfully arrive as
/// `Function`; functions that blow up arrive as `Error` (so one bad function
/// degrades to a placeholder rather than killing the whole dump).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// A successfully decompiled (or, in list mode, simply enumerated) function.
    Function(FunctionInfo),
    /// A function that failed to decompile.
    Error(FunctionError),
    /// A fatal, worker-level problem (bad binary, angr import failure, ...).
    /// The orchestrator surfaces this and treats the worker as failed.
    Fatal { message: String },
    /// One-time, binary-level metadata. Every worker emits it; the
    /// orchestrator keeps the first.
    Meta {
        /// objdump/BFD-style file format, e.g. `elf64-x86-64`.
        file_format: String,
    },
    /// Coarse progress from the build worker (loading / CFG recovery).
    Progress {
        /// Stage label: `load`, `cfg`, or `save`.
        stage: String,
        /// Percentage complete for the stage, when known (CFG recovery).
        #[serde(default)]
        percent: Option<f64>,
    },
    /// How many functions this worker is about to process (its stripe size).
    /// The orchestrator sums these to learn the grand total.
    Plan { count: usize },
    /// The full, address-sorted candidate function list (metadata only, no
    /// text). Emitted once by worker 0 so the TUI can show every function with
    /// a placeholder before any decompilation finishes. Ignored by the plain
    /// dump path.
    Catalog { functions: Vec<FunctionInfo> },
}

/// Metadata plus (optionally) decompiled text for one function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionInfo {
    /// Entry address of the function.
    pub addr: u64,
    /// Human-readable name (e.g. `main`, `sub_401d30`).
    pub name: String,
    /// Size in bytes as recovered by the CFG (0 if unknown).
    #[serde(default)]
    pub size: u64,
    /// Whether the function is a PLT stub.
    #[serde(default)]
    pub is_plt: bool,
    /// Name of the section the function lives in (e.g. `.text`), if known.
    #[serde(default)]
    pub section: Option<String>,
    /// Decompiled C-like source. `None` in list mode.
    #[serde(default)]
    pub text: Option<String>,
    /// Wall-clock time the decompiler spent on this function, in milliseconds.
    /// `None` in list mode (or if the function failed).
    #[serde(default)]
    pub decompile_ms: Option<f64>,
    /// Semantic highlight spans `[start, len, kind]` into `text`, from angr's
    /// codegen position map. Empty unless highlighting was requested.
    #[serde(default)]
    pub spans: Vec<HlSpan>,
}

/// One semantic highlight span: `(start, length, kind)` over a function's text,
/// deserialized from the worker's `[start, len, "kind"]` JSON arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlSpan(pub usize, pub usize, pub String);

/// A function that could not be decompiled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionError {
    pub addr: u64,
    pub name: String,
    /// Short description of the failure.
    pub message: String,
}
