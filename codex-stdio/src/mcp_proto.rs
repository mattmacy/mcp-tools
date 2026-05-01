//! JSON-RPC method names and error codes for the outer MCP server
//! loop the shim exposes on stdio.
//!
//! MCP (Model Context Protocol) speaks newline-delimited JSON-RPC 2.0
//! over stdio (NOT LSP-style `Content-Length` framing — MCP and LSP
//! share the JSON-RPC core but use different transports).

#![allow(missing_docs)]

/// JSON-RPC method names the MCP server understands.
pub mod method {
    /// Server-info handshake. Replied to with `serverInfo` +
    /// `capabilities` advertising `tools` support.
    pub const INITIALIZE: &str = "initialize";
    /// Client-side notification fired after a successful `initialize`.
    /// Per JSON-RPC 2.0 §4.1 a notification has no `id` and the
    /// server MUST NOT reply.
    pub const INITIALIZED: &str = "notifications/initialized";
    /// List the tool surface this shim exposes.
    pub const TOOLS_LIST: &str = "tools/list";
    /// Invoke one tool. `params.name` selects the tool;
    /// `params.arguments` carries its arguments object.
    pub const TOOLS_CALL: &str = "tools/call";
    /// Request the shim shut down. Replies with `{"result": null}`.
    pub const SHUTDOWN: &str = "shutdown";
}

/// JSON-RPC error codes (subset of the spec the shim emits).
pub mod code {
    /// Method exists but arguments were malformed.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Internal server error (catch-all backend failure).
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Parse error (malformed JSON on stdin).
    pub const PARSE_ERROR: i64 = -32700;
}
