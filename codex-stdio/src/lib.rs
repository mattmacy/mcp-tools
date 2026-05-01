//! `codex-stdio` — stdio MCP server delegating to the OpenAI
//! `codex` CLI as a worker backend for MCP-aware clients.
//!
//! ## Why a shim, not native `codex mcp-server` directly
//!
//! `@openai/codex` ships a `codex mcp-server` stdio mode exposing a
//! `codex` + `codex-reply` tool pair. This shim wraps it to provide
//! a stable surface (`codex_health`, `codex_run_task`) so callers
//! do not have to track breaking changes to the upstream MCP
//! tool shape; when upstream churns, the shim is re-pinned once
//! instead of every caller.
//!
//! ## Tool surface
//!
//! Two tools, plus the standard MCP handshake (`initialize`,
//! `tools/list`, `shutdown`):
//!
//! | tool | input | output |
//! |------|-------|--------|
//! | `codex_health` | `{}` | `{available, model, latency_ms, reason?}` |
//! | `codex_run_task` | `{task_packet, worktree_path, max_tokens?}` | `{diff, log, tokens_used}` |
//!
//! ## Security model — defense in depth
//!
//! 1. **Worktree-cwd boundary check** (this crate, [`run_task`]). The
//!    `worktree_path` argument MUST canonicalize under the configured
//!    worktree-root prefix ([`WORKTREE_ROOT_PREFIX`]). Path-escape via
//!    `..` or symlink fails closed before any OpenAI call is made.
//!    This is the bring-up baseline; richer lease enforcement
//!    (allowed_paths / forbidden_paths) is delegated to a co-deployed
//!    lease-aware tool such as `wtpool`.
//! 2. **PostToolUse path-glob hook** (out of scope for this crate).
//!    A co-deployed hook should re-resolve every diff path via
//!    `realpath` post-hoc.
//! 3. **Reviewer-against-lease** (out of scope for this crate).
//!
//! ## `OPENAI_API_KEY` handling
//!
//! Read from env on every request, NOT cached at startup. Missing key
//! degrades [`codex_health`] to `{available: false}` and causes
//! [`codex_run_task`] to return a structured error. The shim never
//! crashes on missing creds; callers detect `available: false` and
//! fall back to whatever alternative path they prefer.
//!
//! ## Replay-fixture path (for tests + smoke without live API)
//!
//! Setting `CODEX_STDIO_REPLAY_FIXTURE=<path-to-json>` redirects the
//! OpenAI HTTP call to a recorded JSON fixture on disk. This lets
//! `cargo test` exercise the wire shape end-to-end without burning
//! tokens or requiring `OPENAI_API_KEY`. The fixture format mirrors a
//! Chat Completions response body verbatim — see
//! [`codex::ReplayClient`].

#![deny(missing_docs)]

pub mod codex;
pub mod health;
pub mod mcp;
pub mod mcp_proto;
pub mod run_task;

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared test helpers. The `ENV_LOCK` mutex serialises any
    //! test that mutates `OPENAI_API_KEY` or
    //! `CODEX_STDIO_REPLAY_FIXTURE` so cargo's default parallel
    //! thread-pool does not race them. Three modules
    //! (`codex::tests`, `health::tests`, `run_task::tests`) all
    //! lock the same `Mutex` so cross-module pairs are also safe.
    use std::sync::Mutex;

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
}

/// Default model the shim invokes when [`run_task::Args::model`] is
/// unset. The `codex` CLI accepts arbitrary model names so this
/// string is just a routing hint, not a binding contract.
pub const DEFAULT_MODEL: &str = "gpt-5.3-codex";

/// Worktree-path prefix the run-task boundary check enforces. Any
/// `worktree_path` argument that does not canonicalize under this
/// prefix is rejected before any HTTP call. Configurable at runtime
/// via the `CODEX_STDIO_WORKTREE_ROOT` env var (see [`run_task`]).
pub const WORKTREE_ROOT_PREFIX: &str = "/tmp/wtpool/";
