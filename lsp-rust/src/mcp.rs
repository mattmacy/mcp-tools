//! MCP (Model Context Protocol) stdio server wrapping the LSP client.
//!
//! Each CLI subcommand maps 1:1 to an MCP tool:
//!
//! | CLI                     | MCP tool                              |
//! |-------------------------|----------------------------------------|
//! | `definition LOC`        | `definition`                           |
//! | `references LOC`        | `references`                           |
//! | `hover LOC`             | `hover`                                |
//! | `workspace-symbols Q`   | `workspace_symbols`                    |
//! | `diagnostics FILE`      | `diagnostics`                          |
//! | `wait-for-indexing`     | `wait_for_indexing`                    |
//!
//! Bug-fix matrix vs zeenix/rust-analyzer-mcp v0.2.0 — this module owns:
//!
//! - **Bug #5 (poll-substring leftover)**: `wait_for_indexing` is its own
//!   tool with explicit `workspace/diagnostic` polling until the result
//!   set stabilizes. No hardcoded substring scan of stderr.
//! - **Structured errors over silent null**: every tool returns either
//!   `{"result": ...}` or
//!   `{"error": {"code": ..., "message": ..., "data": {"error_kind": ...}}}`
//!   — never a bare null on failure.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::lsp::{LspShimError, RustAnalyzerClient};
use lsp_shim_core::mcp_proto::{code, method};

/// Convenience: run the MCP server on stdin/stdout against a workspace.
/// Used by the `serve-mcp` CLI subcommand.
pub fn serve_stdio(workspace: PathBuf, cli_timeout: Option<u64>) -> std::io::Result<()> {
    let backend = RustAnalyzerClient::new(workspace, cli_timeout);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock(), backend)
}

/// Run the MCP server loop on the given stdio handles until EOF.
/// Splitting from `serve_stdio` lets tests drive the loop with in-memory
/// pipes against a constructed (but un-spawned) backend.
pub fn serve<R, W>(reader: R, mut writer: W, mut backend: RustAnalyzerClient) -> std::io::Result<()>
where
    R: Read,
    W: Write,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(trimmed, &mut backend) {
            let body = serde_json::to_string(&response).unwrap_or_else(|e| {
                format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"failed to serialize response: {e}"}}}}"#
                )
            });
            writeln!(writer, "{body}")?;
            writer.flush()?;
        }
    }
}

fn handle_message(line: &str, backend: &mut RustAnalyzerClient) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                code::PARSE_ERROR,
                format!("malformed JSON: {e}"),
                None,
            ));
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = request.get("id").is_none();
    let method = match request.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => {
            if is_notification {
                return None;
            }
            return Some(error_response(
                id,
                code::INVALID_PARAMS,
                "missing `method` field".into(),
                None,
            ));
        }
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method.as_str() {
        method::INITIALIZE => Some(success_response(id, initialize_result())),
        method::INITIALIZED => None,
        method::TOOLS_LIST => Some(success_response(id, tools_list_result())),
        method::TOOLS_CALL => Some(handle_tools_call(id, params, backend)),
        method::SHUTDOWN => {
            let _ = backend.shutdown();
            Some(success_response(id, Value::Null))
        }
        other => {
            if is_notification {
                None
            } else {
                Some(error_response(
                    id,
                    code::METHOD_NOT_FOUND,
                    format!("unknown method: {other}"),
                    Some(json!({ "error_kind": "method_not_found" })),
                ))
            }
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "lsp-rust",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `tools/list` response body. Six tools, strict `inputSchema` per tool —
/// the model cannot dispatch a malformed call.
fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "definition",
                "description": "Resolve the symbol at `file:line:column` (1-based) to its definition site(s) via rust-analyzer's textDocument/definition.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to a .rs file under the indexed workspace." },
                        "line": { "type": "integer", "description": "1-based line number." },
                        "column": { "type": "integer", "description": "1-based column number." }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "references",
                "description": "Find every reference to the symbol at `file:line:column` via rust-analyzer's textDocument/references.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "column": { "type": "integer" }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "hover",
                "description": "Return rust-analyzer's hover documentation for the symbol at `file:line:column`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "column": { "type": "integer" }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "workspace_symbols",
                "description": "Fuzzy workspace-wide symbol search via workspace/symbol.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Substring or fuzzy match against symbol name." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "diagnostics",
                "description": "Pull current diagnostic set for one file via textDocument/diagnostic (LSP 3.17 pull-diagnostics).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" }
                    },
                    "required": ["file"]
                }
            },
            {
                "name": "wait_for_indexing",
                "description": "Block until rust-analyzer's symbol-index output stabilises across two successive samples. Replaces zeenix wrapper's stderr substring scan with an explicit protocol-driven poll.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn handle_tools_call(id: Value, params: Value, backend: &mut RustAnalyzerClient) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => {
            return error_response(
                id,
                code::INVALID_PARAMS,
                "tools/call missing `name`".into(),
                None,
            );
        }
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    let backend_result: std::result::Result<Value, LspShimError> = match name.as_str() {
        "definition" => call_position(backend, &args, PositionKind::Definition),
        "references" => call_position(backend, &args, PositionKind::References),
        "hover" => call_position(backend, &args, PositionKind::Hover),
        "workspace_symbols" => call_workspace_symbols(backend, &args),
        "diagnostics" => call_diagnostics(backend, &args),
        "wait_for_indexing" => backend.wait_for_indexing(),
        other => {
            return error_response(
                id,
                code::METHOD_NOT_FOUND,
                format!("unknown tool: {other}"),
                Some(json!({ "error_kind": "unknown_tool" })),
            );
        }
    };

    match backend_result {
        Ok(payload) => success_response(
            id,
            json!({
                "content": [
                    { "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap_or_default() }
                ],
                "isError": false,
            }),
        ),
        Err(e) => {
            let kind = error_kind(&e);
            let mut data = json!({ "error_kind": kind });
            if let LspShimError::Lsp {
                code: lsp_code,
                retry_after_ms,
                ..
            } = &e
            {
                data["lsp_code"] = json!(lsp_code);
                if let Some(ms) = retry_after_ms {
                    data["retry_after_ms"] = json!(ms);
                }
            }
            error_response(id, code::INTERNAL_ERROR, format!("{e}"), Some(data))
        }
    }
}

enum PositionKind {
    Definition,
    References,
    Hover,
}

fn call_position(
    backend: &mut RustAnalyzerClient,
    args: &Value,
    kind: PositionKind,
) -> std::result::Result<Value, LspShimError> {
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| LspShimError::Protocol("missing `file` (string)".into()))?;
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| LspShimError::Protocol("missing `line` (integer)".into()))?
        as u32;
    let column = args
        .get("column")
        .and_then(Value::as_u64)
        .ok_or_else(|| LspShimError::Protocol("missing `column` (integer)".into()))?
        as u32;
    let path = PathBuf::from(file);
    match kind {
        PositionKind::Definition => backend.definition(&path, line, column),
        PositionKind::References => backend.references(&path, line, column),
        PositionKind::Hover => backend.hover(&path, line, column),
    }
}

fn call_workspace_symbols(
    backend: &mut RustAnalyzerClient,
    args: &Value,
) -> std::result::Result<Value, LspShimError> {
    let query = args.get("query").and_then(Value::as_str).ok_or_else(|| {
        LspShimError::Protocol("workspace_symbols requires `query` (string)".into())
    })?;
    backend.workspace_symbols(query)
}

fn call_diagnostics(
    backend: &mut RustAnalyzerClient,
    args: &Value,
) -> std::result::Result<Value, LspShimError> {
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| LspShimError::Protocol("diagnostics requires `file` (string)".into()))?;
    backend.diagnostics(&PathBuf::from(file))
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: String, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(data) = data {
        error.as_object_mut().unwrap().insert("data".into(), data);
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
}

/// Stable string tag for every `LspShimError` variant. Mirrors the CLI's
/// `error_kind()` so callers consuming both transports see the same
/// taxonomy.
fn error_kind(e: &LspShimError) -> &'static str {
    match e {
        LspShimError::Spawn { .. } => "spawn",
        LspShimError::Lsp { .. } => "lsp",
        LspShimError::Timeout { .. } => "timeout",
        LspShimError::Protocol(_) => "protocol",
        LspShimError::Io(_) => "io",
        LspShimError::Json(_) => "json",
        LspShimError::Internal(_) => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_backend() -> RustAnalyzerClient {
        // No spawn: tests that don't need a live ra construct a client
        // pointing at a nonexistent workspace; the server loop only
        // touches the backend for tools/call, never for initialize/list.
        RustAnalyzerClient::new("/tmp/nonexistent-lsp-rust-test", None)
    }

    #[test]
    fn tools_list_enumerates_six_tools_with_schemas() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6, "expected 6 tools, got {}", tools.len());
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            [
                "definition",
                "references",
                "hover",
                "workspace_symbols",
                "diagnostics",
                "wait_for_indexing"
            ]
        );
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let result = initialize_result();
        assert_eq!(result["serverInfo"]["name"], "lsp-rust");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["protocolVersion"].is_string());
    }

    #[test]
    fn malformed_json_returns_parse_error_not_silent_null() {
        let input = b"this is not json\n".to_vec();
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(input.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("malformed JSON"));
        // Structured-error contract: never a silent `result` on failure.
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn tools_call_unknown_tool_returns_structured_error() {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"does_not_exist","arguments":{{}}}}}}{}"#,
            "\n"
        );
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(request.as_bytes(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(parsed["error"]["data"]["error_kind"], "unknown_tool");
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn tools_list_request_returns_six_tools() {
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n".to_vec();
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(request.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 2);
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6);
    }

    #[test]
    fn initialize_request_does_not_spawn_rust_analyzer() {
        // Initialize must complete using only protocol-layer state — if
        // it tried to spawn against a nonexistent workspace we'd surface
        // a Spawn error here. Backend.spawn() is only invoked on
        // tools/call, never during the MCP handshake.
        let request =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n".to_vec();
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(request.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["serverInfo"]["name"], "lsp-rust");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn initialized_notification_returns_no_response() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n".to_vec();
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(input.as_slice(), &mut output, backend).expect("serve loop");
        // Notifications must not produce a reply per JSON-RPC 2.0 §4.1.
        assert!(
            output.is_empty(),
            "notification produced unexpected response: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn tools_call_definition_routes_to_backend() {
        // Backend points at a nonexistent workspace + uses a bogus
        // rust-analyzer binary so the spawn fails deterministically.
        // What we're testing: the tools/call dispatch reaches the
        // definition handler (otherwise we'd get METHOD_NOT_FOUND).
        std::env::set_var("LSP_RUST_ANALYZER", "/definitely/not/real/ra");
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"definition\",\"arguments\":{\"file\":\"/tmp/x.rs\",\"line\":1,\"column\":1}}}\n".to_vec();
        let mut output = Vec::new();
        let backend = stub_backend();
        serve(request.as_slice(), &mut output, backend).expect("serve loop");
        std::env::remove_var("LSP_RUST_ANALYZER");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 9);
        // Routed to backend → got a Spawn error (kind=spawn) NOT
        // METHOD_NOT_FOUND. This proves dispatch reached the handler.
        assert_eq!(parsed["error"]["code"], code::INTERNAL_ERROR);
        assert_eq!(parsed["error"]["data"]["error_kind"], "spawn");
        assert!(parsed.get("result").is_none());
    }
}
