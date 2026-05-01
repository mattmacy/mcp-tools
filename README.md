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

## Build

```sh
cargo build --release -p codex-stdio
cargo build --release -p wtpool
```

Binaries land at `target/release/{codex-stdio,wtpool}`.

## Test

```sh
cargo test --release
```

## License

Apache-2.0. See [LICENSE](./LICENSE).
