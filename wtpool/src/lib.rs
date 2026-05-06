//! Library surface for `wtpool`.
//!
//! Exposes the same modules the binary uses so integration tests under
//! `tests/` can drive the MCP loop and the tool handlers directly
//! without forking a child process.
//!
//! The four read-only tools (all cached for 60s) are:
//!
//! - [`git`] backs `worktree_list` + `worktree_state`. Walks the repo's
//!   linked worktrees via `git2::Repository::worktrees()`, reports
//!   tip-sha + commits-ahead-of-main + dirty + last-N-log-lines.
//! - [`agents`] backs `agent_inflight_summary`. Cross-references the
//!   per-session task-output files written by long-running CLI agents
//!   AND `/tmp/agent-<task-id>.progress` heartbeat sentinels.
//! - [`reviews`] backs `pending_review`. Stats canonical reviewer
//!   verdict-file paths under `/tmp/` and parses the first-line
//!   verdict word.
//!
//! `cache` provides the 60-second TTL `Mutex<HashMap>` used by all
//! tool entrypoints. `mcp` is the JSON-RPC stdio server loop.

#![deny(missing_docs)]

pub mod agents;
pub mod cache;
/// Compatibility helpers for legacy environment variable names.
pub mod compat;
pub mod cumulative_md;
pub mod git;
pub mod git_exec;
pub mod lease;
pub mod mcp;
pub mod mcp_proto;
pub mod merge;
pub mod pool;
pub mod reviews;

/// Default repo root the MCP server initialises against. Overridden by
/// `WTPOOL_REPO` env var or `--repo` CLI flag.
pub const DEFAULT_REPO: &str = "/repo";
