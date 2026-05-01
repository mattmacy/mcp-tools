# lsp-rust

Long-lived MCP-over-LSP shim around `rust-analyzer`. Replaces
`zeenix/rust-analyzer-mcp` v0.2.0.

## Why

Five confirmed bugs in the upstream wrapper motivated owning a thin
shim instead of vendor-and-patch:

1. **Silent `null` on LSP error.** Agents could not distinguish
   "request failed" from "request succeeded with null result" (e.g.
   cursor on whitespace).
2. **Zero retry on `ContentModified` (-32801) / `ServerCancelled`
   (-32802).** Transient indexing-race errors surfaced as permanent
   failures.
3. **Hardcoded 30 s timeout** — no env var, no flag, no config.
4. **Invisible logging** — failures vanished unless rust-analyzer
   itself crashed.
5. **Hardcoded poll-substring scan of stderr** to decide "indexing
   finished" — brittle, broke every time rust-analyzer touched its
   log format.

## What ships

- `src/rpc.rs` — JSON-RPC framing primitives re-exported from
  `lsp-shim-core` (`encode_frame`, `parse_response` with
  `RpcOutcome::Result | Error` discriminant, `parse_content_length`,
  `send_frame`, `recv_frame`).
- `src/lsp.rs` — `RustAnalyzerClient` with `spawn`
  (initialize/initialized handshake), `request_with_retry` (3x on
  -32801/-32802 with 500 ms backoff), per-method handlers
  (`definition`, `references`, `hover`, `workspace_symbols`,
  `diagnostics`, `wait_for_indexing`), `shutdown`.
- `src/mcp.rs` — MCP stdio server (`initialize`, `tools/list`,
  `tools/call`, `shutdown`) with structured errors carrying
  `error_kind` in `data`.
- `src/main.rs` — `clap` CLI dispatching every subcommand to the LSP
  client; `serve-mcp` hands off to `mcp::serve_stdio`.
- `src/lib.rs` — re-exports `lsp`, `mcp`, `rpc` for integration
  tests.

## Building

```
cargo build --release -p lsp-rust
cargo test --release -p lsp-rust
```

The binary lands at `target/release/lsp-rust`.

## Running (CLI debug mode)

```
lsp-rust --workspace /path/to/project definition path/to/file.rs:7:14
lsp-rust --workspace /path/to/project workspace-symbols MyType
lsp-rust --workspace /path/to/project wait-for-indexing
```

Defaults:

- workspace = `LSP_PROJECT` env var or current dir
- timeout = `LSP_TIMEOUT_SECS` (60 s)
- log file = `LSP_LOG_FILE` (`/tmp/lsp-rust.log`)
- rust-analyzer binary = `LSP_RUST_ANALYZER` (default looks up
  `rust-analyzer` on PATH)

## MCP server mode

```
lsp-rust serve-mcp
```

Speaks newline-delimited JSON-RPC 2.0 on stdio. Register in the host
MCP config (`.mcp.json`, etc.) by pointing at the binary path with
the `serve-mcp` argument.
