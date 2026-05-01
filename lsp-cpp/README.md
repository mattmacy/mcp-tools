# lsp-cpp

Thin MCP-over-LSP shim around `clangd-19`. Replaces buggy upstream
clangd-MCP forks (the `mattmacy/mcp-cpp` fork the dev container used
to install via `cargo install --git`, etc.).

## Why this crate exists

1. **Reduce fork-maintenance.** Every clangd flag tweak in a fork
   requires a fork commit + reinstall. In-tree means edit-and-rebuild.
2. **Standardise with `lsp-rust`.** The sibling rust-analyzer shim
   needs the same `LspBackend` trait surface, the same JSON-RPC
   framer, and the same structured error vocabulary. Two shim crates
   sharing one core crate is cheaper than two forks of upstream MCP
   servers.

## Architecture

- One long-lived clangd process per shim. `Clangd::spawn` does the
  `initialize` handshake; subsequent calls reuse the same process.
- Optional post-`initialize` seed-didOpen. After the LSP `initialized`
  notification, `Clangd::spawn` sends `textDocument/didOpen` for every
  header listed in the `LSP_CPP_SEED_HEADERS` env var (see below).
  This is a perf-tuning knob: pre-warming a known set of high-traffic
  headers populates clangd's in-memory symbol index immediately, so
  the first `workspace_symbol` query resolves those types without
  waiting for `--background-index` shards to land on disk. didOpen is
  best-effort per file — a missing or unreadable header is logged to
  the configured log file and skipped, never fails spawn. Default is
  empty; when unset the shim simply skips this step.
- `LspBackend` trait (`backend.rs`) — minimum surface for the four
  operations actually served (definition, references, hover,
  workspace_symbol). The sibling `lsp-rust` crate implements the same
  surface against rust-analyzer.
- Narrow `compile_commands.json` preferred over the full DB
  (`Clangd::resolve_compile_commands_dir`). Useful when the full DB
  has 100k+ entries and only a subset is interesting.
- Persistent index via `--background-index`. Mounting
  `.cache/clangd/index/` as a named volume in a container lets the
  index survive restarts.
- Structured errors (`ShimError`) — every observable failure mode has
  a typed variant. The CLI prints the error as JSON on stderr so MCP
  callers can surface them instead of seeing the previous "silent
  null" mode of the upstream fork.
- Bounded admission queue (`queue::BoundedQueue`, default depth 16)
  gates `tools/call` dispatch. When clangd is mid-indexing a heavy
  preamble and requests pile up, the wrapper sheds load with
  `error_kind: "queue_depth_exceeded"` rather than queuing unbounded
  or surfacing as broken-pipe.
- Busy-vs-broken classification on per-request timeout. When the
  60 s (position queries) / 30 s (workspace_symbol) deadline fires,
  the wrapper probes `child.try_wait()`: alive subprocess →
  `error_kind: "clangd_busy"` (caller retries with longer timeout);
  exited subprocess → `error_kind: "clangd_exited"` (a supervisor
  consumes this as restart signal).

## Subcommands

```
lsp-cpp probe                                       # smoke-test
lsp-cpp workspace-symbol <query>
lsp-cpp definition <file> <line> <column>
lsp-cpp references <file> <line> <column>
lsp-cpp hover <file> <line> <column>
lsp-cpp build-full-index                            # pre-warm index
lsp-cpp serve-mcp                                   # MCP stdio server
```

`--project <path>` overrides the project root (default from
`LSP_PROJECT`).

`--mode narrow|full|hybrid` selects the indexing strategy
(default `narrow` via `LSP_CPP_INDEX_MODE`):

- `narrow` — clangd indexes the narrow `compile_commands.json` live.
- `full` — clangd reads a pre-built index file only (sub-second
  queries, no live indexer).
- `hybrid` — pre-built index seeds clangd, live indexer fills gaps.

`--index-file <path>` overrides the pre-built index location
(default `$HOME/.cache/lsp-cpp-full-index/index.idx` via
`LSP_CPP_INDEX_FILE`).

## Environment variables

- `LSP_PROJECT` — project root containing `compile_commands.json`.
- `LSP_CPP_INDEX_MODE` — `narrow` | `full` | `hybrid` (default
  `narrow`).
- `LSP_CPP_INDEX_FILE` — pre-built index path for `full` / `hybrid`
  (default `$HOME/.cache/lsp-cpp-full-index/index.idx`).
- `LSP_CPP_SEED_HEADERS` — comma-separated absolute paths of headers
  to send `textDocument/didOpen` for immediately after `initialized`.
  Default empty (seed-didOpen disabled). Use this when you have a
  known set of high-traffic headers and want the first
  `workspace_symbol` query to resolve their types without waiting for
  `--background-index` shards to land. Each header parse can take
  several seconds on a cold cache; keep the list short.
- `CLANGD_BIN` — clangd binary (default `clangd-19`).
- `CLANGD_LOG` — clangd stderr log (default `/tmp/clangd.log`).
- `CLANGD_JOBS` — clangd worker count (default: half of nproc, min 2).

## Tests

`cargo test -p lsp-cpp` exercises the framing layer, narrow-vs-full
DB selection, location-array parsing, and the spawn-time structured-
error paths.

`tests/seed_didopen.rs` is a live integration test gated behind
`LSP_CPP_LIVE_TEST=1` plus `LSP_CPP_SEED_HEADERS=...` plus
`LSP_CPP_QUERY_SYMBOL=<symbol-name>`. It spawns the `lsp-cpp
serve-mcp` binary, drives the MCP handshake, then polls
`tools/call workspace_symbol "<symbol>"` until the seed-didOpen
parses settle. Without seed-didOpen the query typically returns `[]`
indefinitely on a cold cache; with it the query resolves in the
expected wall-time budget.
