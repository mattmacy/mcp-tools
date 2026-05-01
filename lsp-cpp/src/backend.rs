//! Backend trait the shim exposes to its callers (CLI + future MCP layer).
//!
//! The trait is intentionally minimal — the four operations actually
//! served by the MCP tool surface today (definition, references,
//! hover, workspace_symbol). Adding more later is cheap; over-fitting
//! the trait now would force every backend (clangd today, rust-analyzer
//! in the sibling `lsp-rust` crate) to implement features no caller
//! uses.

use crate::error::Result;
use serde::{Deserialize, Serialize};

/// A source location returned by `definition` / `references`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// Absolute file path (clangd returns `file://` URIs; the shim
    /// strips the scheme so callers do not need to URL-decode).
    pub path: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// A symbol returned by `workspace_symbol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// Symbol name as reported by clangd.
    pub name: String,
    /// LSP `SymbolKind` integer (1=File, 5=Class, 12=Function, …).
    pub kind: u32,
    /// Defining file location.
    pub location: Location,
    /// Optional containerName (namespace, parent class, …).
    pub container: Option<String>,
}

/// A hover result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hover {
    /// Plain-text or markdown body.
    pub contents: String,
}

/// Operations every LSP-shim backend must support.
///
/// `&mut self` everywhere because clangd's connection state is
/// inherently stateful (request IDs are monotonic, responses arrive in
/// order). Callers that need concurrent access must wrap the backend in
/// their own synchronisation primitive.
pub trait LspBackend {
    /// Spawn the underlying language server and run the `initialize`
    /// handshake. Idempotent — calling twice on the same backend is a
    /// no-op after the first success.
    fn spawn(&mut self) -> Result<()>;

    /// Resolve the symbol under the cursor at `path:line:column` to its
    /// definition site(s). Multiple results are possible (overload sets,
    /// templates).
    fn definition(&mut self, path: &str, line: u32, column: u32) -> Result<Vec<Location>>;

    /// Find every reference to the symbol under the cursor.
    fn references(&mut self, path: &str, line: u32, column: u32) -> Result<Vec<Location>>;

    /// Return clangd's hover documentation for the symbol under the
    /// cursor, if any.
    fn hover(&mut self, path: &str, line: u32, column: u32) -> Result<Option<Hover>>;

    /// Workspace-wide symbol search.
    fn workspace_symbol(&mut self, query: &str) -> Result<Vec<Symbol>>;

    /// Graceful shutdown — sends `shutdown` + `exit`, waits briefly,
    /// then SIGKILLs if the process is still up.
    fn shutdown(&mut self) -> Result<()>;
}
