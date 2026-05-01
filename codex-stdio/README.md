# codex-stdio

Stdio MCP server delegating to the OpenAI `codex` CLI as a worker
backend for any MCP-aware client (e.g. Claude Code, custom routers).

## Why a shim, not native `codex mcp-server` directly

`@openai/codex` ships its own `codex mcp-server` stdio mode exposing
`codex` + `codex-reply` tools. This shim wraps that, registering a
stable `codex_health` + `codex_run_task` surface so callers do not
have to track breaking changes in the upstream tool shape.

| Server | Tools | Purpose |
|---|---|---|
| `codex-stdio` | `codex_health`, `codex_run_task` | Stable shim surface for callers |
| `codex` (upstream) | `codex`, `codex-reply` | Direct native access |

When upstream ships breaking changes on the native MCP surface, the
shim is re-pinned once instead of every caller. The `codex_run_task`
tool implementation invokes `codex exec --cd <wt> --sandbox
workspace-write` under the hood.

## Tool surface

| Tool | Input | Output |
|---|---|---|
| `codex_health` | `{}` | `{available, model, latency_ms, transport, binary_path?, reason?}` |
| `codex_run_task` | `{task_packet, worktree_path, max_tokens?, model?}` | `{diff, log, tokens_used: {prompt, completion, total}}` |

`codex_health` does NOT round-trip a real OpenAI call — it reports
whether a transport can be built. Production: locates the `codex`
binary on disk via `CODEX_STDIO_BIN` → `which codex` →
`/usr/bin/codex`. Test: presence of `CODEX_STDIO_REPLAY_FIXTURE`.

`codex_run_task` accepts a task packet (opaque prompt body) and a
worktree path. The worktree path MUST canonicalize under the
configured worktree-root prefix (default `/tmp/wtpool/`, override
via `CODEX_STDIO_WORKTREE_ROOT`). Symlink escapes and relative
paths are rejected before any spawn.

## Auth + env vars

The `codex` CLI handles auth itself via ChatGPT OAuth
(`~/.codex/auth.json`) or an API key (`codex login --with-api-key`).
The shim does NOT plumb `OPENAI_API_KEY`.

| Var | Required | Purpose |
|---|---|---|
| `CODEX_STDIO_BIN` | no | Override the codex binary search. Default order: `CODEX_STDIO_BIN` → `which codex` → `/usr/bin/codex`. |
| `CODEX_STDIO_MODEL` | no | Default model (overridden by per-call `model` arg). Defaults to `gpt-5.3-codex`. |
| `CODEX_STDIO_REPLAY_FIXTURE` | for tests | Absolute path to a recorded Chat Completions response JSON. When set, the shim reads from disk instead of spawning `codex exec`. |
| `CODEX_STDIO_SANDBOX_BYPASS` | no | When `1`/`true`, replaces `--sandbox workspace-write` with `--dangerously-bypass-approvals-and-sandbox`. Use only in trusted sandboxes where bwrap is unavailable. |
| `CODEX_STDIO_WORKTREE_ROOT` | no | Override the worktree-root prefix the boundary check enforces. Defaults to `/tmp/wtpool/`. |

If the codex binary is unreachable, `codex_health` returns
`{available: false, reason: "..."}` and `codex_run_task` returns an
MCP error with the same message. The shim never crashes on missing
codex; callers detect `available: false` and fall back as needed.

## CLI

```
codex-stdio serve-mcp                       # JSON-RPC 2.0 stdio MCP server
codex-stdio health                          # one-shot mirror of codex_health
codex-stdio probe                           # smoke test (no network)
echo "fix foo" | codex-stdio run-task --worktree /tmp/wtpool/wt-XX
```

## Build + install

```sh
cargo build --release -p codex-stdio
sudo cp target/release/codex-stdio /usr/local/bin/
```

Register in your MCP client's config (e.g. `.mcp.json`) as
`/usr/local/bin/codex-stdio serve-mcp`.

## Smoke tests

### Shim wire shape (no network, no codex CLI)

```sh
CODEX_STDIO_REPLAY_FIXTURE=$PWD/codex-stdio/tests/fixtures/replay/single-line-fix.json \
  cargo run -q -p codex-stdio -- probe
```

Expected:

```json
{
  "tools_count": 2,
  "health": {
    "available": true,
    "model": "gpt-5.3-codex",
    "latency_ms": 0,
    "transport": "replay",
    "binary_path": "(replay fixture)"
  }
}
```

### Live codex CLI (network, real tokens)

```sh
echo "What is 2+2? Reply with only the number." | \
  /usr/bin/codex exec --cd /tmp --sandbox read-only --skip-git-repo-check \
    --color never --output-last-message /tmp/msg.txt -
cat /tmp/msg.txt
```

## Tests

```sh
cargo test -p codex-stdio
```

Env-mutation tests share a process-global `ENV_LOCK` mutex so
cargo's parallel thread-pool does not race them.

## Security model

Layered, in increasing order of trust (only layer 1 is in this
crate):

1. **Worktree-cwd boundary check** — `validate_worktree_path` in
   `src/run_task.rs`. Canonicalize + `starts_with(<configured
   prefix>)`. Passes through to `codex exec --cd <wt>`.
2. **Post-exec path-glob hook** (out of scope here) — re-resolve
   every diff path via `realpath`.
3. **Reviewer-against-lease** (out of scope here) — verify the
   diff respects any per-task lease constraints.
