//! JSON-RPC 2.0 framing for LSP traffic.
//!
//! This module is now a thin re-export of [`lsp_shim_core::framing`]
//! — the actual implementation lives in the shared `lsp-shim-core`
//! crate alongside the matching code from `lsp-cpp`. Both
//! shims used to carry near-identical copies of the
//! `Content-Length`-framed sender / receiver and the JSON-RPC response
//! parser; convergence to one source of truth eliminates drift risk
//! after each merge to `main`.
//!
//! Re-exporting (rather than `use`-ing at the call sites) keeps the
//! `crate::rpc::*` paths in `lsp.rs` working unchanged and preserves
//! the public API of `lsp-rust` (the integration tests under
//! `tests/` import from this module).
//!
//! Bug-fix matrix vs zeenix/rust-analyzer-mcp v0.2.0 — see the README.
//! The shared crate carries:
//!
//! - **Bug #1 (silent null on LSP error)**: `parse_response` returns
//!   `RpcOutcome::Error { code, message, data }` distinct from
//!   `RpcOutcome::Result(value)` — callers cannot conflate "succeeded
//!   with null" and "errored". Zeenix wrapper coerced both to bare null.
//! - **Bug #2 (no retry)**: `LSP_ERROR_CONTENT_MODIFIED` /
//!   `LSP_ERROR_SERVER_CANCELLED` constants exposed for the retry
//!   layer in `lsp.rs` to match against. Framing layer surfaces the
//!   code; retry policy lives one layer up.

pub use lsp_shim_core::framing::{
    encode_frame, parse_content_length, parse_response, recv_frame, send_frame, Request,
    RpcOutcome, LSP_ERROR_CONTENT_MODIFIED, LSP_ERROR_SERVER_CANCELLED,
};
