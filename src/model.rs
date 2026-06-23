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
        /// The angr version in use, e.g. `9.2.221`.
        #[serde(default)]
        angr_version: Option<String>,
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
    /// Set when this function was served from cache and was decompiled by a
    /// different angr version than the one in use now: the cached version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_version: Option<String>,
}

/// One semantic highlight span: `(start, length, kind)` over a function's text,
/// deserialized from the worker's `[start, len, "kind"]` JSON arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlSpan(pub usize, pub usize, pub String);

/// The comment placed above a function served from a cache that a different
/// angr version produced.
pub fn stale_comment(cached: &str, current: Option<&str>) -> String {
    match current {
        Some(cur) => format!("// cached: decompiled with angr {cached} (current: {cur})"),
        None => format!("// cached: decompiled with angr {cached}"),
    }
}

/// The cached full function catalog, letting warm runs skip Python.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogCache {
    #[serde(default)]
    pub angr_version: String,
    #[serde(default)]
    pub file_format: Option<String>,
    pub functions: Vec<FunctionInfo>,
}

/// One cached function decompilation (`decomp/0x<addr>.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct DecompCache {
    pub text: String,
    #[serde(default)]
    pub spans: Vec<HlSpan>,
    #[serde(default)]
    pub decompile_ms: Option<f64>,
    /// The angr version that produced this decompilation.
    #[serde(default)]
    pub angr_version: Option<String>,
}

/// A function that could not be decompiled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionError {
    pub addr: u64,
    pub name: String,
    /// Short description of the failure.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_comment_with_and_without_current() {
        assert_eq!(
            stale_comment("9.0.0", Some("9.2.221")),
            "// cached: decompiled with angr 9.0.0 (current: 9.2.221)"
        );
        assert_eq!(
            stale_comment("9.0.0", None),
            "// cached: decompiled with angr 9.0.0"
        );
    }

    #[test]
    fn decomp_cache_reads_angr_version_and_defaults() {
        let d: DecompCache =
            serde_json::from_str(r#"{"text":"x","angr_version":"9.1.0"}"#).unwrap();
        assert_eq!(d.angr_version.as_deref(), Some("9.1.0"));
        assert!(d.spans.is_empty());
        // Older cache files without the field deserialize to None.
        let d2: DecompCache = serde_json::from_str(r#"{"text":"y"}"#).unwrap();
        assert_eq!(d2.angr_version, None);
    }

    #[test]
    fn hlspan_from_array() {
        let s: HlSpan = serde_json::from_str("[3, 4, \"type\"]").unwrap();
        assert_eq!((s.0, s.1, s.2.as_str()), (3, 4, "type"));
    }

    #[test]
    fn decompcache_defaults() {
        let d: DecompCache = serde_json::from_str("{\"text\":\"x\"}").unwrap();
        assert_eq!(d.text, "x");
        assert!(d.spans.is_empty());
        assert!(d.decompile_ms.is_none());
    }
}
