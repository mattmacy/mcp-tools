//! Compatibility helpers for legacy environment variable names.
//!
//! Routes the canonical `LSP_*` / `LSP_CPP_*` env vars through
//! [`lsp_shim_core::compat::env_or_legacy`] so the legacy
//! `LURE_LSP_*` / `LURE_LSP_CPP_*` names continue to resolve with a
//! one-shot stderr deprecation note.

use lsp_shim_core::compat::env_or_legacy;

/// Resolve the project-root env var with the legacy fallback.
pub fn project_env() -> Option<String> {
    env_or_legacy("LSP_PROJECT", "LURE_LSP_PROJECT")
}

/// Resolve the index-mode env var with the legacy fallback.
pub(crate) fn index_mode_env() -> Option<String> {
    env_or_legacy("LSP_CPP_INDEX_MODE", "LURE_LSP_CPP_INDEX_MODE")
}

/// Resolve the index-file env var with the legacy fallback.
pub(crate) fn index_file_env() -> Option<String> {
    env_or_legacy("LSP_CPP_INDEX_FILE", "LURE_LSP_CPP_INDEX_FILE")
}

/// Resolve the live-test gate env var with the legacy fallback.
pub fn live_test_env() -> Option<String> {
    env_or_legacy("LSP_CPP_LIVE_TEST", "LURE_LSP_CPP_LIVE_TEST")
}
