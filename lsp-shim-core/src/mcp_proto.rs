//! JSON-RPC method names and error codes for the outer MCP server
//! loop both shims expose on stdio.
//!
//! MCP (Model Context Protocol) speaks newline-delimited JSON-RPC 2.0
//! over stdio (NOT LSP-style `Content-Length` framing — MCP and LSP
//! share the JSON-RPC core but use different transports). The five
//! method names below are exactly what Claude Code's `.mcp.json`
//! runtime sends; the four error codes are the JSON-RPC standard
//! subset both shims emit. Centralised here so adding a sixth method
//! (or a new error code) does not require touching two crates.

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
    /// Request the shim shut down its backend (LSP server). Replies
    /// with `{"result": null}` regardless of backend exit status.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_match_mcp_spec() {
        assert_eq!(method::INITIALIZE, "initialize");
        assert_eq!(method::INITIALIZED, "notifications/initialized");
        assert_eq!(method::TOOLS_LIST, "tools/list");
        assert_eq!(method::TOOLS_CALL, "tools/call");
        assert_eq!(method::SHUTDOWN, "shutdown");
    }

    #[test]
    fn error_codes_match_jsonrpc_spec() {
        // Standard JSON-RPC 2.0 codes — these are stable and any
        // drift would silently break MCP clients matching on them.
        assert_eq!(code::PARSE_ERROR, -32700);
        assert_eq!(code::METHOD_NOT_FOUND, -32601);
        assert_eq!(code::INVALID_PARAMS, -32602);
        assert_eq!(code::INTERNAL_ERROR, -32603);
    }
}
