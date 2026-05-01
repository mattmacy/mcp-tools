//! Shared primitives for the LSP MCP shims.
//!
//! Both `lsp-rust` (rust-analyzer backend) and `lsp-cpp`
//! (clangd backend) historically re-implemented the same JSON-RPC framing
//! over `Content-Length`-prefixed stdio that LSP mandates, plus the same
//! JSON-RPC method-name and error-code constants for the MCP outer
//! transport. The two implementations were byte-identical on the
//! framing functions and divergent only in which error variant the
//! `Result` returned. Drift across the two copies (one shim adding a
//! framing fix, the other not) was a real risk on every merge, so this
//! crate consolidates the wire layer.
//!
//! Modules:
//!
//! - [`framing`]  — LSP `Content-Length` send/recv on top of
//!   `std::io::Read` / `std::io::Write`, plus JSON-RPC response parsing
//!   ([`framing::RpcOutcome`]) that distinguishes a successful `null`
//!   result from a server-side error response. Returns `std::io::Error`
//!   so each shim can map into its own typed error via `From`.
//! - [`mcp_proto`] — JSON-RPC method names and error codes the outer
//!   MCP server loop in each shim emits (`tools/list`, `tools/call`,
//!   `INVALID_PARAMS`, `METHOD_NOT_FOUND`, `INTERNAL_ERROR`,
//!   `PARSE_ERROR`).
//!
//! Out of scope (deferred to a follow-up convergence branch):
//!
//! - The `LspBackend` trait. The cpp shim defines one; the rust shim's
//!   `RustAnalyzerClient` does not implement it because its public
//!   methods take `&Path`/`PathBuf` and return `serde_json::Value`
//!   rather than the typed `Vec<Location>` / `Vec<Symbol>` / `Hover`
//!   the cpp trait specifies. Unifying signatures is a non-trivial
//!   API change on both shims.
//! - The MCP `serve` loop. Generic-over-backend would force the trait
//!   convergence above; deferred along with it.

#![deny(missing_docs)]

pub mod framing;
pub mod mcp_proto;
