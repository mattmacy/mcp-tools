//! MCP stdio server loop with the Codex-side tool surface
//! (two tools: `codex_health`, `codex_run_task`).
//!
//! Newline-delimited JSON-RPC 2.0; method names + error codes live in
//! the local [`crate::mcp_proto`] module.

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{json, Value};

use crate::mcp_proto::{code, method};
use crate::{health, run_task};

/// Run the MCP server on stdin/stdout. Returns when stdin reaches EOF.
pub fn serve_stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock())
}

/// Run the MCP server loop on the given stdio handles. Pulled out as
/// a separate fn so integration tests can drive request bytes through
/// it without forking a child process.
pub fn serve<R, W>(reader: R, mut writer: W) -> std::io::Result<()>
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
        if let Some(response) = handle_message(trimmed) {
            let body = serde_json::to_string(&response).unwrap_or_else(|e| {
                format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"failed to serialize: {e}"}}}}"#
                )
            });
            writeln!(writer, "{body}")?;
            writer.flush()?;
        }
    }
}

fn handle_message(line: &str) -> Option<Value> {
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
    let method_name = match request.get("method").and_then(Value::as_str) {
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

    match method_name.as_str() {
        method::INITIALIZE => Some(success_response(id, initialize_result())),
        method::INITIALIZED => None,
        method::TOOLS_LIST => Some(success_response(id, tools_list_result())),
        method::TOOLS_CALL => Some(handle_tools_call(id, params)),
        method::SHUTDOWN => Some(success_response(id, Value::Null)),
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

/// `initialize` reply — advertises tools capability + server identity.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "codex-stdio",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `tools/list` reply — strict schemas per tool.
pub fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "codex_health",
                "description": "Reports whether the Codex worker backend can serve requests right now. Callers invoke this before classifying a task to decide whether the Codex tier is in the candidate set. Does NOT round-trip a real OpenAI call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "codex_run_task",
                "description": "Dispatch one task packet to OpenAI Chat Completions. Returns {diff, log, tokens_used}. Worktree path MUST canonicalize under the configured worktree-root prefix (default /tmp/worktrees/, override via CODEX_STDIO_WORKTREE_ROOT) — Codex is constrained to operate inside its assigned worktree. Set CODEX_STDIO_REPLAY_FIXTURE=<path> in env to replay a recorded response (test/smoke path).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_packet": {
                            "type": "string",
                            "description": "Prompt body sent to the model. Format opaque to this shim; the routing skill is the canonical producer."
                        },
                        "worktree_path": {
                            "type": "string",
                            "description": "Absolute path the Codex worker is constrained to. Validated via canonicalize + prefix check."
                        },
                        "max_tokens": {
                            "type": "integer",
                            "description": "Output ceiling (max_completion_tokens). Default 16384."
                        },
                        "model": {
                            "type": "string",
                            "description": "Override the default model. Defaults to CODEX_STDIO_MODEL env var, then gpt-5.3-codex."
                        }
                    },
                    "required": ["task_packet", "worktree_path"]
                }
            }
        ]
    })
}

fn handle_tools_call(id: Value, params: Value) -> Value {
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

    let result: Result<Value, String> = match name.as_str() {
        "codex_health" => health::run(),
        "codex_run_task" => run_task::run(&args),
        other => {
            return error_response(
                id,
                code::METHOD_NOT_FOUND,
                format!("unknown tool: {other}"),
                Some(json!({ "error_kind": "unknown_tool" })),
            );
        }
    };

    match result {
        Ok(payload) => success_response(
            id,
            json!({
                "content": [
                    { "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap_or_default() }
                ],
                "isError": false,
            }),
        ),
        Err(e) => error_response(
            id,
            code::INTERNAL_ERROR,
            e,
            Some(json!({ "error_kind": "tool_failure" })),
        ),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: String, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error.as_object_mut().unwrap().insert("data".into(), data);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_advertises_two_tools_with_schemas() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["codex_health", "codex_run_task"]);
        for t in tools {
            assert!(t["inputSchema"].is_object(), "tool missing schema: {t}");
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        // codex_run_task must mark task_packet + worktree_path as
        // required so a malformed dispatch fails at the schema check.
        let run_task = &tools[1];
        let required = run_task["inputSchema"]["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.iter().any(|v| v == "task_packet"));
        assert!(required.iter().any(|v| v == "worktree_path"));
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let r = initialize_result();
        assert_eq!(r["serverInfo"]["name"], "codex-stdio");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let input = b"this is not json\n".to_vec();
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn unknown_tool_returns_structured_error() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"does_not_exist\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(parsed["error"]["data"]["error_kind"], "unknown_tool");
    }

    #[test]
    fn initialized_notification_returns_no_response() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();
        assert!(
            output.is_empty(),
            "notification produced reply: {:?}",
            output
        );
    }
}
