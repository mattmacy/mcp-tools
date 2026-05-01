//! JSON-RPC method names and error codes for the outer MCP server
//! loop the `wtpool` binary exposes on stdio.
//!
//! MCP (Model Context Protocol) speaks newline-delimited JSON-RPC 2.0
//! over stdio.

#![allow(missing_docs)]

/// JSON-RPC method names the MCP server understands.
pub mod method {
    /// Server-info handshake.
    pub const INITIALIZE: &str = "initialize";
    /// Client-side notification fired after a successful `initialize`.
    pub const INITIALIZED: &str = "notifications/initialized";
    /// List the tool surface this server exposes.
    pub const TOOLS_LIST: &str = "tools/list";
    /// Invoke one tool.
    pub const TOOLS_CALL: &str = "tools/call";
    /// Request the server shut down.
    pub const SHUTDOWN: &str = "shutdown";
}

/// JSON-RPC error codes (subset of the spec the server emits).
pub mod code {
    /// Method exists but arguments were malformed.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Internal server error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Parse error (malformed JSON on stdin).
    pub const PARSE_ERROR: i64 = -32700;
}
