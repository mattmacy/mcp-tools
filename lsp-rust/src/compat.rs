//! Compatibility helpers for legacy environment variable names.
//!
//! Routes the canonical `LSP_*` env vars through
//! [`lsp_shim_core::compat::env_or_legacy`] so the legacy
//! `LURE_LSP_*` names continue to resolve with a one-shot stderr
//! deprecation note. The helper is shared with `lsp-cpp` and `wtpool`
//! so all three shims emit the same note shape.

use lsp_shim_core::compat::env_or_legacy;

/// Resolve the workspace-root env var with the legacy fallback.
pub fn project_env() -> Option<String> {
    env_or_legacy("LSP_PROJECT", "LURE_LSP_PROJECT")
}

/// Resolve the rust-analyzer binary env var with the legacy fallback.
pub(crate) fn rust_analyzer_env() -> Option<String> {
    env_or_legacy("LSP_RUST_ANALYZER", "LURE_LSP_RUST_ANALYZER")
}

/// Resolve the timeout env var with the legacy fallback.
pub(crate) fn timeout_env() -> Option<String> {
    env_or_legacy("LSP_TIMEOUT_SECS", "LURE_LSP_TIMEOUT_SECS")
}

/// Resolve the log-file env var with the legacy fallback.
pub(crate) fn log_file_env() -> Option<String> {
    env_or_legacy("LSP_LOG_FILE", "LURE_LSP_LOG_FILE")
}
