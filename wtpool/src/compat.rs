//! Compatibility helpers for legacy environment variable names.
//!
//! Routes the canonical `WTPOOL_*` env vars through
//! [`lsp_shim_core::compat::env_or_legacy`] so the legacy
//! `LURE_WORKTREE_*` names continue to resolve with a one-shot
//! stderr deprecation note.

use lsp_shim_core::compat::env_or_legacy;

/// Resolve the repo-root env var with the legacy fallback.
pub fn repo_root_env() -> Option<String> {
    env_or_legacy("WTPOOL_REPO", "LURE_WORKTREE_REPO")
}
