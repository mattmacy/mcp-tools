//! Legacy environment-variable name resolution shared across the
//! mcp-tools shims.
//!
//! Exposes [`env_or_legacy`], the single helper each shim's per-crate
//! `compat` module wraps with project-specific function pairs (e.g.
//! `LSP_PROJECT` / `LURE_LSP_PROJECT`). Centralising the read +
//! deprecation-note bookkeeping here avoids three byte-identical
//! copies of the same fn — the lure-export overlay used to ship
//! that triplicate as `flywheel-*/src/compat.rs`.
//!
//! Behaviour:
//!
//! 1. Read `canonical` (e.g. `LSP_PROJECT`); on hit, return
//!    `Some(value)` immediately.
//! 2. Otherwise read `legacy` (e.g. `LURE_LSP_PROJECT`); on hit emit a
//!    one-shot stderr deprecation note (deduped per legacy name across
//!    the process) and return `Some(value)`.
//! 3. Both unset → `None`.
//!
//! Zero-cost when neither var is set (two `getenv` calls + nothing
//! allocated). The dedup table is lazy-initialised on first hit of any
//! legacy var.
//!
//! Dedup is keyed on the `legacy` name so an operator who exports
//! several legacy aliases at once (e.g. `LURE_LSP_PROJECT` AND
//! `LURE_WORKTREE_REPO`) sees one note per alias rather than one note
//! per process.

use std::collections::HashSet;
use std::sync::Mutex;

/// Read `canonical`, falling back to `legacy` with a one-time stderr
/// deprecation note PER LEGACY NAME.
pub fn env_or_legacy(canonical: &str, legacy: &str) -> Option<String> {
    if let Ok(v) = std::env::var(canonical) {
        return Some(v);
    }
    if let Ok(v) = std::env::var(legacy) {
        static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
        let mut guard = WARNED.lock().expect("compat::WARNED mutex poisoned");
        let set = guard.get_or_insert_with(HashSet::new);
        if set.insert(legacy.to_string()) {
            eprintln!("note: {legacy} is deprecated, use {canonical}");
        }
        return Some(v);
    }
    None
}
