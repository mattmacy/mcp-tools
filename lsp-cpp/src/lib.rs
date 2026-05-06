//! LSP shim around clangd-19.
//!
//! Replaces buggy upstream clangd-MCP forks with an in-tree shim so:
//!
//! - clangd flags and index settings stay version-locked to this
//!   crate's branch,
//! - the `LspBackend` trait is shared with the sibling
//!   `lsp-rust` crate (rust-analyzer backend),
//! - structured error reporting replaces the previous "silent null"
//!   mode the fork used when clangd returned an empty `result` array.
//!
//! ## Architecture
//!
//! - One long-lived clangd process per shim. Spawned on first request,
//!   re-used for every subsequent request, killed on graceful shutdown.
//!   Avoids the per-query startup cost (PCH parse, compile-commands scan)
//!   that dominates wall time for small queries.
//! - Narrow compile_commands.json. The shim looks for
//!   `compile_commands.narrow.json` next to the full DB and prefers it
//!   when present.
//! - Persistent index volume. clangd's `--background-index` writes into
//!   the project's `.cache/clangd/index/` directory; mounting that as
//!   a named volume in a container lets the index survive restarts.
//! - Optional seed-didOpen on initialize. Configurable list of headers
//!   warmed via `textDocument/didOpen` so the first `workspace/symbol`
//!   query resolves common types without waiting for `--background-index`
//!   shards to land on disk. See `LSP_CPP_SEED_HEADERS` in the README.
//!
//! ## Why a trait
//!
//! The sibling `lsp-rust` crate exposes the same
//! `definition` / `references` / `hover` / `workspace_symbol` surface
//! against rust-analyzer. The trait lets a future shared MCP loop
//! drive either backend; today each shim ships its own loop and
//! re-uses only the framing/proto primitives in `lsp-shim-core`.

#![deny(missing_docs)]

pub mod backend;
pub mod clangd;
/// Compatibility helpers for legacy environment variable names.
pub mod compat;
pub mod error;
pub mod jsonrpc;
pub mod mcp;
pub mod queue;
pub mod supervisor;

pub use backend::{Hover, Location, LspBackend, Symbol};
pub use error::ShimError;
