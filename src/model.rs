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
    /// Decompiled C-like source. `None` in list mode.
    #[serde(default)]
    pub text: Option<String>,
}

/// A function that could not be decompiled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionError {
    pub addr: u64,
    pub name: String,
    /// Short description of the failure.
    pub message: String,
}
