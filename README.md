# mcp-tools

A small workspace of standalone MCP (Model Context Protocol) server
binaries plus a clangd launcher, all licensed Apache-2.0.

## Subprojects

### [`codex-stdio/`](./codex-stdio)

Stdio MCP server that wraps the OpenAI `codex` CLI as a worker
backend for any MCP-aware client. Exposes a stable
`codex_health` + `codex_run_task` tool surface so callers do not
have to track breaking changes in the upstream `codex mcp-server`
tool shape. Auth flows through the `codex` CLI itself; the shim
never handles `OPENAI_API_KEY` directly.

Pins: `clap ~4.6`, `serde ~1.0`. Targets the `codex` CLI binary
shipped by `npm i -g @openai/codex` (any version supporting
`codex exec --json`).

### [`wtpool/`](./wtpool)

Read-only stdio MCP server + CLI exposing per-worktree git state,
in-flight CLI-agent telemetry, a cooperative worktree lease pool,
and a guarded `merge_to_main` orchestrator. Replaces the
`worktree-list / log main..HEAD / status --porcelain / agent-tail`
Bash dance every cascade-merge planning round tends to pay.

Pins: `git2 ~0.20` (vendored libgit2), `fs2 ~0.4`, `tempfile ~3.27`,
`thiserror ~1.0`, `clap ~4.6`.

### [`clangd-launcher/`](./clangd-launcher)

Bash + Python launcher that starts `clangd` in
background-index mode against a large C++ codebase whose
`compile_commands.json` is pre-generated. Includes:

- `start-clangd.sh` / `stop-clangd.sh` — idempotent wrappers
- `clangd-driver.py` — keeps the LSP session warm and feeds clangd
  TUs to crawl
- `build-clangd-patched.sh` — optional builder for a clangd variant
  that adds `--background-index-memory-limit` (apply patches via
  `PATCH_DIR`)

Set `PROJECT_ROOT` (or the legacy `UE_ROOT`) to the directory
containing `compile_commands.json`.

### [`lsp-shim-core/`](./lsp-shim-core)

Shared library used by `lsp-rust` and `lsp-cpp`. Owns the LSP
`Content-Length`-framed JSON-RPC wire I/O (`send_frame`,
`recv_frame`, `parse_response` with the success-vs-error
discriminant) plus the JSON-RPC method-name and error-code constants
the MCP outer transport emits (`initialize`, `tools/list`,
`tools/call`, `INVALID_PARAMS`, `METHOD_NOT_FOUND`, …). Extracted
from byte-identical copies in the two LSP shims so a framing fix
lands once instead of twice.

### [`lsp-rust/`](./lsp-rust)

Long-lived MCP-over-LSP shim around `rust-analyzer`. Replaces
`zeenix/rust-analyzer-mcp` v0.2.0. Fixes its silent-null-on-error,
no-retry-on-`ContentModified`, hardcoded-30s-timeout,
invisible-logging, and substring-scan-of-stderr bugs. Exposes the
`definition` / `references` / `hover` / `workspace_symbols` /
`diagnostics` / `wait_for_indexing` operations as both a CLI
(per-subcommand) and an MCP stdio server (`lsp-rust serve-mcp`).
Returns structured errors carrying `error_kind` in the JSON-RPC
`error.data` field so callers can distinguish "succeeded with null"
from "request failed".

Pins: `clap ~4.6`. Spawns the `rust-analyzer` binary on PATH (or the
`LSP_RUST_ANALYZER` override).

### [`lsp-cpp/`](./lsp-cpp)

Thin MCP-over-LSP shim around `clangd-19`. Replaces buggy upstream
clangd-MCP forks. Long-lived clangd subprocess, persistent
`--background-index`, narrow-vs-full `compile_commands.json`
selection, structured errors, bounded admission queue with
busy-vs-broken classification, and an optional post-`initialize`
seed-didOpen step driven by `LSP_CPP_SEED_HEADERS`.

Pins: `anyhow ~1.0`. Spawns `clangd-19` on PATH (or the `CLANGD_BIN`
override).

## Build

```sh
cargo build --release --workspace
```

Binaries land at `target/release/{codex-stdio,wtpool,lsp-rust,lsp-cpp}`.

## Test

```sh
cargo test --release --workspace
```

## License

Apache-2.0. See [LICENSE](./LICENSE).
