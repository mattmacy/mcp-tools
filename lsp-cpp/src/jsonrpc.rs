//! Minimal LSP framing for clangd traffic.
//!
//! The actual `Content-Length`-prefixed JSON-RPC framer lives in the
//! shared `lsp-shim-core::framing` module (originally extracted
//! from byte-identical copies in this crate and the sibling
//! `lsp-rust` rust-analyzer crate). This wrapper preserves the
//! cpp shim's `crate::error::Result` return type and the existing
//! `ShimError::Protocol` semantics by remapping the shared
//! `io::ErrorKind::InvalidData` framer errors into `ShimError::Protocol`
//! at the boundary. Other errors (truncated body → `UnexpectedEof`,
//! genuine I/O failures) flow through `ShimError::Io` via the existing
//! `#[from] io::Error` conversion on the error enum.
//!
//! Why preserve the wrapper rather than rewrite `clangd.rs` to use
//! `io::Result` directly: every clangd-side caller wants a
//! `Protocol(...)` error variant for malformed framing, distinct from
//! the catch-all `Io(...)` variant. The shared framer is intentionally
//! transport-typed (`io::Error`) so each shim maps to its own taxonomy
//! at the edge.
//!
//! clangd never initiates a request, only replies to ours and emits
//! unsolicited notifications (which we ignore for now), so the shared
//! framer's request-id demuxing is unused on the cpp side — we only
//! call `send` / `recv`.

use crate::error::{Result, ShimError};
use lsp_shim_core::framing;
use std::io::{self, BufRead, Write};

/// Write a JSON value as an LSP-framed message.
pub fn send<W: Write>(out: W, payload: &serde_json::Value) -> Result<()> {
    framing::send_frame(out, payload).map_err(into_shim_error)
}

/// Read one LSP-framed message, returning the parsed JSON value.
pub fn recv<R: BufRead>(input: R) -> Result<serde_json::Value> {
    framing::recv_frame(input).map_err(into_shim_error)
}

/// Map a framing-layer `io::Error` into the cpp shim's `ShimError`
/// taxonomy. `InvalidData` (missing `Content-Length`, bad JSON body,
/// non-numeric length) becomes `Protocol(...)` — those are framing
/// bugs that match the legacy "protocol error" semantics callers rely
/// on. Everything else (`UnexpectedEof` from a truncated body, genuine
/// I/O failure on the socket) flows through `ShimError::Io` so callers
/// see the original `io::Error` chain.
fn into_shim_error(err: io::Error) -> ShimError {
    if err.kind() == io::ErrorKind::InvalidData {
        ShimError::Protocol(err.to_string())
    } else {
        ShimError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_through_in_memory_buffer() {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"processId": null}
        });
        let mut buf = Vec::new();
        send(&mut buf, &payload).expect("send ok");
        let mut cursor = Cursor::new(buf);
        let read = recv(&mut cursor).expect("recv ok");
        assert_eq!(payload, read);
    }

    #[test]
    fn missing_content_length_returns_protocol_error() {
        let bytes = b"\r\n{}".to_vec();
        let mut cursor = Cursor::new(bytes);
        let err = recv(&mut cursor).expect_err("must fail without Content-Length");
        assert!(matches!(err, ShimError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn truncated_body_surfaces_io_error() {
        // Header claims 100 bytes, body has 2.
        let bytes = b"Content-Length: 100\r\n\r\n{}".to_vec();
        let mut cursor = Cursor::new(bytes);
        let err = recv(&mut cursor).expect_err("must fail on truncated body");
        // read_exact maps short read to UnexpectedEof -> Io variant.
        assert!(matches!(err, ShimError::Io(_)), "got {err:?}");
    }
}
