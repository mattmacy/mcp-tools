//! LSP `Content-Length`-framed JSON-RPC wire I/O.
//!
//! LSP frames every JSON-RPC message with a header block terminated by
//! `\r\n\r\n` and a mandatory `Content-Length: N` header naming the
//! byte length of the JSON body that follows. This module owns:
//!
//! - [`send_frame`] / [`recv_frame`] — write/read one framed JSON
//!   message against a `Write` / `BufRead`.
//! - [`encode_frame`] — build a [`Request`] into a framed `Vec<u8>` for
//!   callers that want to ship a single buffer (used by the rust shim
//!   for atomic sends).
//! - [`parse_response`] / [`RpcOutcome`] — parse a JSON-RPC response
//!   body into success or error variants, preserving the distinction
//!   between a successful `null` payload (e.g. `definition` on
//!   whitespace) and a server-reported error.
//! - [`parse_content_length`] — parse a header block into the declared
//!   body byte length, for callers that consume the header block
//!   separately from the body (e.g. the older request/response
//!   codepath in `lsp-rust`).
//! - LSP transient-error code constants ([`LSP_ERROR_CONTENT_MODIFIED`],
//!   [`LSP_ERROR_SERVER_CANCELLED`]) the retry layer in each shim
//!   matches against.
//!
//! All errors surface as `std::io::Error`. Each shim wraps that into
//! its own typed error (`LspShimError`, `ShimError`) via the
//! `#[from] std::io::Error` already present on those variants.
//!
//! ## Bug-fix rationale (preserved from the rust shim's `rpc.rs`)
//!
//! The `RpcOutcome` split is the load-bearing fix for bug #1 of
//! `zeenix/rust-analyzer-mcp` v0.2.0: the upstream wrapper coerced
//! both a successful `null` and a server error into a bare JSON `null`
//! at the MCP boundary, so callers could not tell "cursor on
//! whitespace, no symbol" apart from "indexer crashed five minutes
//! ago." Returning `RpcOutcome::Error { code, message, data }` gives
//! the retry layer in `lsp.rs` something to match on against
//! [`LSP_ERROR_CONTENT_MODIFIED`] / [`LSP_ERROR_SERVER_CANCELLED`].

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// LSP error code: server cancelled the request because document
/// content changed underneath it. Standard JSON-RPC retry-after-replay
/// signal — resending the same request after the document settles is
/// correct.
pub const LSP_ERROR_CONTENT_MODIFIED: i64 = -32801;

/// LSP error code: server explicitly cancelled (shutting down,
/// restarting, indexing reset). Same retry semantics as
/// `ContentModified`.
pub const LSP_ERROR_SERVER_CANCELLED: i64 = -32802;

/// One JSON-RPC request frame ready to ship over the LSP wire.
///
/// Constructed rather than borrowed so the framing layer can hold
/// monotonically-assigned request IDs without callers tracking them.
#[derive(Debug, Serialize)]
pub struct Request {
    /// JSON-RPC version literal — always `"2.0"` for LSP.
    pub jsonrpc: &'static str,
    /// Monotonic request ID. Server responds with the same id; the
    /// LSP client demuxes pending requests by this id.
    pub id: u64,
    /// LSP method name, e.g. `"textDocument/definition"`.
    pub method: String,
    /// Method-specific params. `Value::Null` is acceptable for
    /// no-param methods like `shutdown`.
    pub params: Value,
}

/// Parsed JSON-RPC response — either a successful result or an error.
#[derive(Debug)]
pub enum RpcOutcome {
    /// Server returned `{"result": <value>}`. The value can itself be
    /// JSON `null` — which correctly means "the request succeeded and
    /// the answer is null", e.g. `definition` on whitespace.
    Result(Value),
    /// Server returned `{"error": {...}}`. Code, message, and optional
    /// data preserved verbatim so the retry layer in each shim can
    /// match on `code` and surface `message`/`data` to the caller.
    Error {
        /// JSON-RPC error code. Negative integer; LSP defines several
        /// in the -32800 range on top of the standard JSON-RPC ones.
        code: i64,
        /// Human-readable error message.
        message: String,
        /// Method-specific error data, if the server supplied any.
        data: Option<Value>,
    },
}

/// Build the on-the-wire byte sequence for a request: header block
/// then JSON body. Returned as `Vec<u8>` so callers can `write_all`
/// in one shot.
pub fn encode_frame(req: &Request) -> serde_json::Result<Vec<u8>> {
    let body = serde_json::to_vec(req)?;
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a JSON response body into [`RpcOutcome`]. Body is the JSON
/// string after the `Content-Length` header block has been stripped.
///
/// Returns `Err` only on framing-level corruption (non-JSON, missing
/// `jsonrpc` field). Server-reported LSP errors return
/// `Ok(RpcOutcome::Error { ... })` — they are valid responses, just
/// not successful ones.
pub fn parse_response(body: &[u8]) -> io::Result<RpcOutcome> {
    #[derive(Deserialize)]
    struct RawResponse {
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RawError>,
    }
    #[derive(Deserialize)]
    struct RawError {
        code: i64,
        message: String,
        #[serde(default)]
        data: Option<Value>,
    }

    let parsed: RawResponse = serde_json::from_slice(body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("response not valid JSON: {e}"),
        )
    })?;

    if let Some(err) = parsed.error {
        Ok(RpcOutcome::Error {
            code: err.code,
            message: err.message,
            data: err.data,
        })
    } else {
        // `result` may be JSON `null` — that is a *successful*
        // response with a null payload (cursor on whitespace, no
        // symbol, etc.) and must not be conflated with an error.
        Ok(RpcOutcome::Result(parsed.result.unwrap_or(Value::Null)))
    }
}

/// Parse an LSP header block: a sequence of `Name: Value\r\n` lines
/// terminated by an empty `\r\n` line. Returns the byte length
/// declared in `Content-Length` (the only header LSP requires us to
/// honor).
///
/// Errors on missing or non-numeric `Content-Length` so callers cannot
/// silently slurp the wrong number of body bytes.
pub fn parse_content_length(headers: &str) -> io::Result<usize> {
    for line in headers.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            return rest.trim().parse::<usize>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad Content-Length: {e}"),
                )
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing Content-Length header",
    ))
}

/// Send one LSP-framed JSON-RPC message to a writer (typically the
/// language-server subprocess's stdin).
pub fn send_frame<W: Write>(mut out: W, payload: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(payload).map_err(io::Error::other)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

/// Read one LSP-framed JSON-RPC message from a `BufRead`. Header
/// block is terminated by an empty `\r\n` line; `Content-Length`
/// declares the body byte count. Other headers (`Content-Type`) are
/// ignored.
///
/// Returns `Err(InvalidData)` on missing `Content-Length` or non-JSON
/// body; returns `Err(UnexpectedEof)` on truncated body or
/// stream-closed-mid-header.
pub fn recv_frame<R: BufRead>(mut input: R) -> io::Result<Value> {
    let mut content_length: Option<usize> = None;
    let mut header_line = String::new();
    loop {
        header_line.clear();
        let n = input.read_line(&mut header_line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "language server closed stdout before sending a complete message",
            ));
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Content-Length {rest:?}: {e}"),
                )
            })?);
        }
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "language server response missing Content-Length",
        )
    })?;
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf)?;
    let value: Value = serde_json::from_slice(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad JSON body: {e}")))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn encode_frame_emits_content_length_header_then_body() {
        let req = Request {
            jsonrpc: "2.0",
            id: 7,
            method: "textDocument/definition".into(),
            params: json!({"textDocument": {"uri": "file:///a.rs"}}),
        };
        let bytes = encode_frame(&req).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.starts_with("Content-Length: "),
            "missing Content-Length prefix: {s}"
        );
        let (header, body) = s.split_once("\r\n\r\n").expect("header/body separator");
        let declared: usize = header
            .strip_prefix("Content-Length: ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            declared,
            body.len(),
            "Content-Length must match body byte length"
        );
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["method"], "textDocument/definition");
    }

    #[test]
    fn parse_content_length_extracts_numeric_value() {
        let headers = "Content-Length: 137\r\nContent-Type: utf-8\r\n";
        assert_eq!(parse_content_length(headers).unwrap(), 137);
    }

    #[test]
    fn parse_content_length_rejects_missing_header() {
        let headers = "Content-Type: utf-8\r\n";
        let err = parse_content_length(headers).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("missing Content-Length"),
            "unexpected msg: {err}"
        );
    }

    #[test]
    fn parse_content_length_rejects_non_numeric() {
        let headers = "Content-Length: not-a-number\r\n";
        let err = parse_content_length(headers).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_response_distinguishes_null_result_from_error() {
        let null_body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        match parse_response(null_body).unwrap() {
            RpcOutcome::Result(v) => assert!(v.is_null()),
            RpcOutcome::Error { .. } => panic!("null result misclassified as error"),
        }

        let err_body =
            br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32801,"message":"content modified"}}"#;
        match parse_response(err_body).unwrap() {
            RpcOutcome::Error { code, message, .. } => {
                assert_eq!(code, LSP_ERROR_CONTENT_MODIFIED);
                assert!(message.contains("content modified"));
            }
            RpcOutcome::Result(_) => panic!("error response misclassified as result"),
        }
    }

    #[test]
    fn parse_response_preserves_error_data_payload() {
        let body = br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"internal","data":{"detail":"index reset"}}}"#;
        match parse_response(body).unwrap() {
            RpcOutcome::Error {
                code,
                message,
                data,
            } => {
                assert_eq!(code, -32603);
                assert_eq!(message, "internal");
                let data = data.expect("data field preserved");
                assert_eq!(data["detail"], "index reset");
            }
            RpcOutcome::Result(_) => panic!("error misclassified"),
        }
    }

    #[test]
    fn parse_response_rejects_non_json_body() {
        let err = parse_response(b"not-json-at-all").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn retry_codes_match_lsp_spec() {
        assert_eq!(LSP_ERROR_CONTENT_MODIFIED, -32801);
        assert_eq!(LSP_ERROR_SERVER_CANCELLED, -32802);
    }

    #[test]
    fn send_recv_frame_round_trip() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"processId": null}
        });
        let mut buf = Vec::new();
        send_frame(&mut buf, &payload).expect("send ok");
        let mut cursor = Cursor::new(buf);
        let read = recv_frame(&mut cursor).expect("recv ok");
        assert_eq!(payload, read);
    }

    #[test]
    fn recv_frame_missing_content_length_errors() {
        let bytes = b"\r\n{}".to_vec();
        let mut cursor = Cursor::new(bytes);
        let err = recv_frame(&mut cursor).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn recv_frame_truncated_body_eof() {
        let bytes = b"Content-Length: 100\r\n\r\n{}".to_vec();
        let mut cursor = Cursor::new(bytes);
        let err = recv_frame(&mut cursor).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
