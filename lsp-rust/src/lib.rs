//! Library surface for `lsp-rust`.
//!
//! The binary at `src/main.rs` is the public entrypoint; this `lib.rs`
//! exposes the same modules so integration tests under `tests/` (and the
//! eventual reviewer-driven smoke harness) can drive the framing and MCP
//! layers directly without forking a child process.

#![deny(missing_docs)]

/// Compatibility helpers for legacy environment variable names.
pub mod compat;
pub mod lsp;
pub mod mcp;
pub mod rpc;
