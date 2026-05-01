//! Structured errors for the LSP shim.
//!
//! The previous upstream clangd-MCP fork returned an empty JSON `result`
//! object on most failure modes (clangd not started, request timed out,
//! malformed compile_commands.json). Callers had no way to distinguish
//! "symbol genuinely not found" from "indexer crashed five minutes ago",
//! and the MCP transport layer happily forwarded the silent null.
//!
//! `ShimError` is the typed alternative: every failure mode that can
//! observably reach a caller has its own variant, and the
//! `Display` impl produces a one-line summary suitable for the
//! `error.message` field of a JSON-RPC response.

use std::io;
use thiserror::Error;

/// Failure modes a shim caller can observe.
#[derive(Debug, Error)]
pub enum ShimError {
    /// `clangd` binary not found on `PATH` or at the `CLANGD_BIN` override.
    #[error("clangd binary not found (looked at {path}); install clangd-19 or set CLANGD_BIN")]
    ClangdMissing {
        /// Path that was probed.
        path: String,
    },

    /// `compile_commands.json` (narrow or full) was not present at the
    /// expected location. clangd will refuse to index TUs without it.
    #[error("compile_commands.json not found under {root}; run a compile_commands narrowing helper or generate the full DB first")]
    NoCompileCommands {
        /// Project root the shim searched.
        root: String,
    },

    /// Pre-built clangd index file (`IndexMode::Full` /
    /// `IndexMode::Hybrid`) was not present at the configured path.
    /// Run the out-of-band builder (`lsp-cpp build-full-index`,
    /// follow-up branch) or set `LSP_CPP_INDEX_FILE` to an
    /// existing index.
    #[error("pre-built clangd index file not found at {path}; build it with `clangd-indexer` or set LSP_CPP_INDEX_MODE=narrow to skip")]
    NoIndexFile {
        /// Path the shim probed.
        path: String,
    },

    /// clangd was spawned but the `initialize` handshake did not produce
    /// a response within the configured timeout.
    #[error("clangd initialize handshake timed out after {timeout_s}s; clangd log at {log_path}")]
    InitializeTimeout {
        /// Configured timeout in seconds.
        timeout_s: u64,
        /// Path to the clangd stderr log for post-mortem.
        log_path: String,
    },

    /// A request that previously succeeded against the same clangd
    /// instance now returns no response. Distinct from
    /// `InitializeTimeout` because the indexer might still be alive but
    /// stuck on a single TU.
    #[error(
        "clangd request timed out after {timeout_s}s ({method}); see {log_path} for last heartbeat"
    )]
    RequestTimeout {
        /// LSP method that timed out.
        method: String,
        /// Configured per-request timeout.
        timeout_s: u64,
        /// Path to the clangd stderr log.
        log_path: String,
    },

    /// clangd is alive but CPU-busy on a long-running task — the
    /// per-request timeout expired while `child.try_wait()` confirms
    /// the subprocess is still running. Distinct from
    /// [`Self::RequestTimeout`] (legacy, doesn't disambiguate
    /// alive-vs-dead) and [`Self::ClangdExited`] (subprocess
    /// terminated). Emitted when an MCP caller hits the per-request
    /// timeout against a subprocess that is mid-indexing (UMG class with
    /// 100 MB preamble takes 18 s+ to parse cold). Caller should retry
    /// with a longer timeout or wait for indexing to complete; this is
    /// NOT a broken-pipe / wrapper-restart event.
    ///
    /// Carries a coarse status string ("indexing", "parsing", "drain")
    /// captured from the most recent log heartbeat, when available, so
    /// callers can show progress to humans rather than a blank "busy".
    #[error("clangd busy after {timeout_s}s ({method}); status={current_status}; log: {log_path}")]
    ClangdBusy {
        /// LSP method that timed out.
        method: String,
        /// Configured per-request timeout that fired.
        timeout_s: u64,
        /// Coarse status tag from the most recent log heartbeat.
        /// Empty string if no heartbeat parse succeeded; callers
        /// should treat empty as "unknown — retry-with-longer".
        current_status: String,
        /// Path to the clangd stderr log so callers can tail for
        /// progress.
        log_path: String,
    },

    /// Admission queue rejected the request because depth was at
    /// capacity. Distinct from [`Self::ClangdBusy`] (clangd is alive
    /// but slow) and [`Self::ClangdExited`] (clangd is dead). Emitted
    /// when the wrapper has too many in-flight requests to safely
    /// accept another without unbounded queueing.
    ///
    /// This is NOT broken-pipe — clangd may be perfectly healthy; the
    /// wrapper itself is shedding load. Caller should retry after
    /// `retry_after_s` seconds.
    #[error(
        "clangd-wrapper queue full ({in_flight}/{capacity}); retry after {retry_after_s}s — \
         this is wrapper-side load shedding, clangd may be healthy"
    )]
    QueueDepthExceeded {
        /// Current depth at time of rejection.
        in_flight: usize,
        /// Configured ceiling.
        capacity: usize,
        /// Suggested retry interval in seconds (advisory).
        retry_after_s: u64,
    },

    /// clangd exited unexpectedly. The wrapper reports the exit status
    /// rather than restarting silently — the caller should decide
    /// whether to retry (typically yes, after surfacing the failure).
    #[error("clangd exited unexpectedly (status={status}); log: {log_path}")]
    ClangdExited {
        /// Process exit status, formatted as `code=N` or `signal=N`.
        status: String,
        /// Path to the clangd stderr log.
        log_path: String,
    },

    /// JSON-RPC framing or payload error. Almost always indicates a
    /// shim bug (bad framing) or a clangd version mismatch.
    #[error("clangd JSON-RPC error: {0}")]
    Protocol(String),

    /// I/O error talking to the clangd subprocess.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// JSON serialisation/deserialisation error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ShimError>;
