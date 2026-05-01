//! MCP (Model Context Protocol) stdio server for `wtpool`.
//!
//! Newline-delimited JSON-RPC 2.0 over stdio, four read-only tools
//! advertised via `tools/list`, every error returned as
//! `{error: {code, message, data}}` rather than a silent null.
//!
//! Tool surface (spec §1.2):
//!
//! | tool                     | input                       | output |
//! |--------------------------|-----------------------------|--------|
//! | `worktree_list`          | `{}`                        | `{worktrees: [...]}` |
//! | `worktree_state`         | `{path: string}`            | `{branch, tip_sha, commits_ahead, files_changed, last_log_lines, untracked_count, dirty}` |
//! | `agent_inflight_summary` | `{stale_minutes?: int}`     | `{worktrees: [...]}` |
//! | `pending_review`         | `{branch: string}`          | `{torvalds, lattner}` |
//! | `merge_to_main`          | spec input             | spec output |

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::agents::agent_inflight_summary;
use crate::cache::TtlCache;
use crate::git::{validate_worktree_path, worktree_list, worktree_state};
use crate::lease::{lease_error_to_mcp, LeaseEmitArgs, Worker, WorktreeLease, LEASE_FILENAME};
use crate::merge::{merge_to_main, MergeRequest};
use crate::pool::{pool_acquire, pool_release, pool_status};
use crate::reviews::pending_review;
use crate::mcp_proto::{code, method};

/// Server config — bound to a single repo root + cache instance for the
/// lifetime of the stdio loop.
pub struct WorktreeServer {
    /// Repository root the server is bound to. Re-opened per call so a
    /// new linked worktree shows up without restarting.
    pub repo_root: PathBuf,
    /// 60-second TTL cache shared across all four tool handlers.
    pub cache: TtlCache,
}

impl WorktreeServer {
    /// Construct against the provided repo root. The repo is not
    /// opened eagerly — every tool call re-opens it via `git2`. That
    /// is intentional: `git2::Repository` is `!Send`, and the stdio
    /// loop wants the freedom to re-read worktree metadata after a
    /// new worktree has been added without restarting the server.
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            cache: TtlCache::new(),
        }
    }
}

/// Convenience: run the MCP server on stdin/stdout against a repo root.
pub fn serve_stdio(repo_root: PathBuf) -> std::io::Result<()> {
    let server = WorktreeServer::new(repo_root);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock(), server)
}

/// Run the MCP server loop on the given stdio handles until EOF.
pub fn serve<R, W>(reader: R, mut writer: W, server: WorktreeServer) -> std::io::Result<()>
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
        if let Some(response) = handle_message(trimmed, &server) {
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

fn handle_message(line: &str, server: &WorktreeServer) -> Option<Value> {
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
        method::TOOLS_CALL => Some(handle_tools_call(id, params, server)),
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
            "name": "wtpool",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `tools/list` reply — strict input schemas per tool.
pub fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "worktree_list",
                "description": "List the workspace's main checkout + every linked worktree, with tip-sha, branch, commits-ahead-of-main, and a dirty flag (any tracked-or-untracked status).",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                }
            },
            {
                "name": "worktree_state",
                "description": "Per-worktree detail: branch, tip-sha, commits-ahead, files-changed (tracked-modified), last 5 log lines from main..HEAD, untracked-count, dirty flag. Path must be an absolute path under /repo.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path to a worktree directory under /repo (or the repo root itself)." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "agent_inflight_summary",
                "description": "Cross-reference /tmp/agent-*/*/tasks/*.output mtimes and /tmp/agent-<task-id>.progress heartbeat sentinels with worktree paths. Returns per-worktree groupings with stale flags.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "stale_minutes": { "type": "integer", "description": "Mtime older than this is reported as stale. Default 5." }
                    }
                }
            },
            {
                "name": "pending_review",
                "description": "Stat /tmp/<branch>-{torvalds,lattner,carmack}.md. Returns existence, mtime, and the first-line verdict word (lowercased) per voice.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch": { "type": "string", "description": "Branch name, e.g. `worktree-state-mcp-shim`." }
                    },
                    "required": ["branch"]
                }
            },
            {
                "name": "merge_to_main",
                "description": "Merge a worktree branch into main. Steps: rebase main, auto-resolve cumulative.md table-row conflicts (heuristic, conservative), compose Reviewed-by trailer, run ALLOW_MAIN_COMMIT=1 git merge --no-ff. Refuses self-merge per reviewer-voice policy (`reviewer_voices` must contain a non-`worktree-worker` voice). `dry_run=true` writes proposed message to /tmp + returns path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch": { "type": "string", "description": "Branch name to merge into main." },
                        "reviewer_voices": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Reviewer identifiers for the Reviewed-by trailer. Must contain at least one non-`worktree-worker` voice (reviewer-voice policy)."
                        },
                        "merge_message_subject": {
                            "type": "string",
                            "description": "Subject line of the merge commit. Must be <=72 chars and non-empty."
                        },
                        "merge_message_body": {
                            "type": "string",
                            "description": "Body of the merge commit (after subject + blank line). Trailer is appended automatically."
                        },
                        "auto_resolve_cumulative_md": {
                            "type": "boolean",
                            "description": "Default true. When true, cumulative.md-only rebase conflicts are auto-resolved via the union-merge heuristic."
                        },
                        "dry_run": {
                            "type": "boolean",
                            "description": "Default false. When true, stops after composing the merge message; no rebase, no merge."
                        },
                        "worktree_path": {
                            "type": "string",
                            "description": "Optional. Absolute path to the linked worktree the branch is checked out in. Defaults to /tmp/wtpool/<branch>."
                        }
                    },
                    "required": ["branch", "reviewer_voices", "merge_message_subject", "merge_message_body"]
                }
            },
            {
                "name": "pool_acquire",
                "description": "Claim the first detached+clean slot under <repo>/wt-pool/wt-*. With `branch_name`: create the branch via `git checkout -b <branch_name> <base_sha>`. Without `branch_name`: drive the slot's detached HEAD to `<base_sha>` so the caller gets a slot at the resolved base, ready for their own branch operation. Either path is atomic with the rebase step — when `base_sha` is omitted the slot lands at *current* main tip even if its pre-existing HEAD was stale. Race-safe via per-slot O_EXCL lockfile. Returns absolute slot path + resolved base sha (and `branch` when supplied). Errors: pool-not-found, pool-exhausted (with in-use list), branch-already-checked-out, base-sha-not-in-repo.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch_name": { "type": "string", "description": "Optional. Branch to create on the acquired slot. Must not already exist in any worktree. When omitted the slot is returned still detached at base_sha." },
                        "base_sha": { "type": "string", "description": "Optional. Commit SHA the branch is created at (or detached HEAD is advanced to). Defaults to current main tip." }
                    }
                }
            },
            {
                "name": "pool_release",
                "description": "Reverse of pool_acquire: detach the slot, hard-reset to main, `git clean -fdx -e .cargo` (drops untracked AND ignored files, preserves the worktree-template `.cargo/config.toml`), then verify post-condition (slot HEAD detached at main_tip AND `git status --porcelain` empty). Refuses if the slot's branch has commits ahead of main not yet merged in (would lose work) OR if the working tree is dirty, unless force=true. Best-effort deletes the now-orphan branch from the main repo's ref store. P0 fix 2026-04-28: prior `-fd` left ignored files (target/, build cruft) behind; classify_slot's is_dirty doesn't see ignored files, so the slot stayed `free` and the next acquire handed out prior-session state. `-e .cargo` keeps the per-worktree target-dir override alive across release cycles.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path to the pool slot to release. Must be under <repo>/wt-pool/." },
                        "force": { "type": "boolean", "description": "Default false. When true, releases even if the branch has unmerged commits." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "pool_status",
                "description": "Census of pool state. Returns {free: [{path, head_sha}], in_use: [{path, branch, commits_ahead}]}.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "worktree_lease_get",
                "description": "Read + validate the lease at <worktree_path>/.wt-lease.json. Returns the parsed lease JSON. Errors with error_kind=\"io\" if the lease is missing, \"invalid_json\"/\"missing_field\"/\"unsupported_schema_version\"/\"invalid_task_id\"/\"invalid_timestamp\" on schema mismatch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "worktree_path": { "type": "string", "description": "Absolute path to the worktree directory hosting .wt-lease.json." }
                    },
                    "required": ["worktree_path"]
                }
            },
            {
                "name": "worktree_lease_emit",
                "description": "Compose + write a fresh worktree lease to <worktree_path>/.wt-lease.json. Validates the worktree path exists. Returns {wrote, task_id, schema_version}. Schema: tools/wtpool/schemas/worktree-lease.v1.json.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "worktree_path": { "type": "string", "description": "Absolute path to the worktree directory the lease scopes." },
                        "task_id": { "type": "string", "description": "Stable task identifier matching `^[A-Za-z0-9][A-Za-z0-9._-]*$`." },
                        "worker": { "type": "string", "enum": ["claude-opus", "claude-sonnet", "claude-haiku", "codex", "human"] },
                        "branch": { "type": "string" },
                        "allowed_paths": { "type": "array", "items": { "type": "string" } },
                        "forbidden_paths": { "type": "array", "items": { "type": "string" } },
                        "test_commands": { "type": "array", "items": { "type": "string" } },
                        "merge_authority": { "type": "string", "description": "Defaults to `review-agent` when omitted." },
                        "expires_at": { "type": "string", "description": "Optional RFC 3339 soft-expiry stamp." },
                        "parent_task_id": { "type": "string", "description": "Optional parent dispatch correlation id." }
                    },
                    "required": ["worktree_path", "task_id", "worker", "branch"]
                }
            },
            {
                "name": "worktree_lease_check",
                "description": "Check whether a single repo-relative `target_path` is permitted by the lease at <worktree_path>/.wt-lease.json. Returns {target, allowed: bool}. Forbidden paths take precedence over allowed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "worktree_path": { "type": "string" },
                        "target_path": { "type": "string", "description": "Repo-relative path to test against the lease's allow + forbid globs." }
                    },
                    "required": ["worktree_path", "target_path"]
                }
            }
        ]
    })
}

fn handle_tools_call(id: Value, params: Value, server: &WorktreeServer) -> Value {
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
        "worktree_list" => {
            let key = "worktree_list".to_string();
            server
                .cache
                .get_or_compute(&key, || worktree_list(&server.repo_root))
        }
        "worktree_state" => {
            let path_raw = match args.get("path").and_then(Value::as_str) {
                Some(p) => p.to_string(),
                None => {
                    return error_response(
                        id,
                        code::INVALID_PARAMS,
                        "worktree_state requires `path` (string)".into(),
                        Some(json!({ "error_kind": "missing_arg" })),
                    );
                }
            };
            match validate_worktree_path(&path_raw) {
                Ok(canon) => {
                    let key = format!("worktree_state:{}", canon.display());
                    server
                        .cache
                        .get_or_compute(&key, || worktree_state(&server.repo_root, &canon))
                }
                Err(e) => Err(e),
            }
        }
        "agent_inflight_summary" => {
            let stale = args
                .get("stale_minutes")
                .and_then(Value::as_u64)
                .unwrap_or(crate::agents::DEFAULT_STALE_MINUTES);
            let key = format!("agent_inflight_summary:{stale}");
            // Real `known_worktrees` so the `associate_via_known`
            // fallback can match observation task-ids that embed a
            // worktree name (common dispatch convention) when the
            // args + JSONL substring scans both miss.
            let known = crate::git::linked_worktree_paths(&server.repo_root);
            server.cache.get_or_compute(&key, || {
                agent_inflight_summary(&server.repo_root, stale, &known)
            })
        }
        "pending_review" => {
            let branch = match args.get("branch").and_then(Value::as_str) {
                Some(b) => b.to_string(),
                None => {
                    return error_response(
                        id,
                        code::INVALID_PARAMS,
                        "pending_review requires `branch` (string)".into(),
                        Some(json!({ "error_kind": "missing_arg" })),
                    );
                }
            };
            let key = format!("pending_review:{branch}");
            server
                .cache
                .get_or_compute(&key, || pending_review(&branch))
        }
        "merge_to_main" => merge_to_main_dispatch(&args, server),
        "pool_acquire" => {
            let branch_name = args.get("branch_name").and_then(Value::as_str);
            let base_sha = args.get("base_sha").and_then(Value::as_str);
            pool_acquire(&server.repo_root, branch_name, base_sha)
        }
        "pool_release" => {
            let path_raw = match args.get("path").and_then(Value::as_str) {
                Some(p) => p.to_string(),
                None => {
                    return error_response(
                        id,
                        code::INVALID_PARAMS,
                        "pool_release requires `path` (string)".into(),
                        Some(json!({ "error_kind": "missing_arg" })),
                    );
                }
            };
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            pool_release(&server.repo_root, std::path::Path::new(&path_raw), force)
        }
        "pool_status" => pool_status(&server.repo_root),
        "worktree_lease_get" => match worktree_lease_get_dispatch(&args) {
            Ok(v) => Ok(v),
            Err((msg, data)) => {
                return error_response(id, code::INTERNAL_ERROR, msg, Some(data));
            }
        },
        "worktree_lease_emit" => match worktree_lease_emit_dispatch(&args) {
            Ok(v) => Ok(v),
            Err((msg, data)) => {
                return error_response(id, code::INTERNAL_ERROR, msg, Some(data));
            }
        },
        "worktree_lease_check" => match worktree_lease_check_dispatch(&args) {
            Ok(v) => Ok(v),
            Err((msg, data)) => {
                return error_response(id, code::INTERNAL_ERROR, msg, Some(data));
            }
        },
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

/// Parse `merge_to_main` args into a [`MergeRequest`] + dispatch.
/// Bypasses the TTL cache: `merge_to_main` mutates state, so cached
/// results would be incorrect.
fn merge_to_main_dispatch(args: &Value, server: &WorktreeServer) -> Result<Value, String> {
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .ok_or("merge_to_main requires `branch` (string)")?
        .to_string();
    let reviewer_voices: Vec<String> = args
        .get("reviewer_voices")
        .and_then(Value::as_array)
        .ok_or("merge_to_main requires `reviewer_voices` (array of strings)")?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let merge_message_subject = args
        .get("merge_message_subject")
        .and_then(Value::as_str)
        .ok_or("merge_to_main requires `merge_message_subject` (string)")?
        .to_string();
    let merge_message_body = args
        .get("merge_message_body")
        .and_then(Value::as_str)
        .ok_or("merge_to_main requires `merge_message_body` (string)")?
        .to_string();
    let auto_resolve_cumulative_md = args
        .get("auto_resolve_cumulative_md")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let req = MergeRequest {
        branch: branch.clone(),
        reviewer_voices,
        merge_message_subject,
        merge_message_body,
        auto_resolve_cumulative_md,
        dry_run,
    };

    // Resolve worktree path: explicit override else default to
    // `<repo>/wt-pool/<branch>` to match the convention every
    // worktree-worker dispatch uses.
    let wt_path: PathBuf = match args.get("worktree_path").and_then(Value::as_str) {
        Some(p) => validate_worktree_path(p)?,
        None => server.repo_root.join("wt-pool").join(&branch),
    };

    merge_to_main(&server.repo_root, &wt_path, &req)
}

/// Resolve `worktree_path` arg, then read + validate the lease at
/// `<worktree>/.wt-lease.json`. Returns the parsed JSON, or an
/// MCP-shaped (message, data) error tuple.
fn worktree_lease_get_dispatch(args: &Value) -> Result<Value, (String, Value)> {
    let worktree = require_string_arg(args, "worktree_path")?;
    let wt = std::path::Path::new(&worktree);
    let lease = WorktreeLease::read_from_worktree(wt).map_err(|e| lease_error_to_mcp(&e))?;
    serde_json::to_value(&lease).map_err(|e| {
        (
            format!("serialize lease: {e}"),
            json!({ "error_kind": "internal_serialize" }),
        )
    })
}

/// Compose + write a lease per the user-supplied args. Mirrors
/// [`crate::lease::LeaseEmitArgs`] but parses each field out of the
/// MCP `arguments` object.
fn worktree_lease_emit_dispatch(args: &Value) -> Result<Value, (String, Value)> {
    let worktree = require_string_arg(args, "worktree_path")?;
    let task_id = require_string_arg(args, "task_id")?;
    let worker_str = require_string_arg(args, "worker")?;
    let branch = require_string_arg(args, "branch")?;
    let worker = match worker_str.as_str() {
        "claude-opus" => Worker::ClaudeOpus,
        "claude-sonnet" => Worker::ClaudeSonnet,
        "claude-haiku" => Worker::ClaudeHaiku,
        "codex" => Worker::Codex,
        "human" => Worker::Human,
        other => {
            return Err((
                format!("unknown worker `{other}`"),
                json!({ "error_kind": "invalid_worker" }),
            ));
        }
    };
    let allowed_paths = optional_string_array(args, "allowed_paths");
    let forbidden_paths = optional_string_array(args, "forbidden_paths");
    let test_commands = optional_string_array(args, "test_commands");
    let merge_authority = args
        .get("merge_authority")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_at = args
        .get("expires_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_task_id = args
        .get("parent_task_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    let emit_args = LeaseEmitArgs {
        task_id,
        worker: Some(worker),
        worktree: std::path::PathBuf::from(&worktree),
        branch,
        allowed_paths,
        forbidden_paths,
        test_commands,
        merge_authority,
        expires_at,
        parent_task_id,
    };
    let lease = emit_args.into_lease().map_err(|e| lease_error_to_mcp(&e))?;
    lease
        .write_to_worktree()
        .map_err(|e| lease_error_to_mcp(&e))?;
    Ok(json!({
        "wrote": format!("{worktree}/{LEASE_FILENAME}"),
        "task_id": lease.task_id,
        "schema_version": lease.schema_version,
    }))
}

/// Read the lease at `<worktree>/.wt-lease.json` and decide whether
/// `target_path` is permitted.
fn worktree_lease_check_dispatch(args: &Value) -> Result<Value, (String, Value)> {
    let worktree = require_string_arg(args, "worktree_path")?;
    let target = require_string_arg(args, "target_path")?;
    let wt = std::path::Path::new(&worktree);
    let lease = WorktreeLease::read_from_worktree(wt).map_err(|e| lease_error_to_mcp(&e))?;
    Ok(json!({
        "target": target,
        "allowed": lease.matches_path(&target),
    }))
}

fn require_string_arg(args: &Value, name: &str) -> Result<String, (String, Value)> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            (
                format!("missing required argument `{name}` (string)"),
                json!({ "error_kind": "missing_arg", "field": name }),
            )
        })
}

fn optional_string_array(args: &Value, name: &str) -> Vec<String> {
    args.get(name)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

    fn server() -> WorktreeServer {
        WorktreeServer::new(PathBuf::from("/repo"))
    }

    #[test]
    fn tools_list_advertises_eleven_tools_with_schemas() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 11);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "worktree_list",
                "worktree_state",
                "agent_inflight_summary",
                "pending_review",
                "merge_to_main",
                "pool_acquire",
                "pool_release",
                "pool_status",
                "worktree_lease_get",
                "worktree_lease_emit",
                "worktree_lease_check",
            ]
        );
        for t in tools {
            assert!(t["inputSchema"].is_object(), "tool missing schema: {t}");
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn merge_to_main_rejects_self_merge_voices() {
        // Subject contains branch substring (Rule 10 guardrail added
        // 2026-04-29) so we exercise the voices-only-worktree-worker
        // refusal, not the subject/branch refusal.
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"tools/call\",\"params\":{\"name\":\"merge_to_main\",\"arguments\":{\"branch\":\"feature-x\",\"reviewer_voices\":[\"worktree-worker\"],\"merge_message_subject\":\"feature-x: voices smoke\",\"merge_message_body\":\"\"}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INTERNAL_ERROR);
        let msg = parsed["error"]["message"].as_str().unwrap();
        assert!(msg.contains("reviewer-voice policy"), "got {msg:?}");
    }

    #[test]
    fn merge_to_main_missing_required_arg_errors() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"tools/call\",\"params\":{\"name\":\"merge_to_main\",\"arguments\":{\"branch\":\"feature-x\"}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INTERNAL_ERROR);
        let msg = parsed["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("reviewer_voices") || msg.contains("merge_message"),
            "got {msg:?}"
        );
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let r = initialize_result();
        assert_eq!(r["serverInfo"]["name"], "wtpool");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let input = b"this is not json\n".to_vec();
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn unknown_tool_returns_structured_error() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"does_not_exist\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(parsed["error"]["data"]["error_kind"], "unknown_tool");
    }

    #[test]
    fn tools_call_pending_review_routes_to_handler() {
        // Use a deliberately-absent branch so the call returns
        // exists=false rather than touching real /tmp state.
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"pending_review\",\"arguments\":{\"branch\":\"_unit_test_branch_qx5\"}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["id"], 11);
        let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inner["torvalds"]["exists"], false);
        assert_eq!(inner["lattner"]["exists"], false);
    }

    #[test]
    fn worktree_state_rejects_bad_path() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"worktree_state\",\"arguments\":{\"path\":\"/etc/passwd\"}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INTERNAL_ERROR);
        let msg = parsed["error"]["message"].as_str().unwrap();
        assert!(msg.contains("/repo"), "got {msg:?}");
    }

    #[test]
    fn worktree_state_missing_path_arg_returns_invalid_params() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"worktree_state\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INVALID_PARAMS);
    }

    #[test]
    fn pending_review_missing_branch_arg_returns_invalid_params() {
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"pending_review\",\"arguments\":{}}}\n";
        let mut output = Vec::new();
        serve(req.as_slice(), &mut output, server()).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INVALID_PARAMS);
    }

    #[test]
    fn worktree_lease_get_missing_file_returns_io_kind() {
        // Worktree exists but no .wt-lease.json => io error.
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().to_path_buf();
        let req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{{\"name\":\"worktree_lease_get\",\"arguments\":{{\"worktree_path\":\"{}\"}}}}}}\n",
            wt.display()
        );
        let mut output = Vec::new();
        let server = WorktreeServer::new(PathBuf::from("/repo"));
        serve(req.as_bytes(), &mut output, server).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::INTERNAL_ERROR);
        assert_eq!(parsed["error"]["data"]["error_kind"], "io");
    }

    #[test]
    fn worktree_lease_emit_then_get_round_trips() {
        // End-to-end via MCP: emit a lease, then read it back.
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().to_path_buf();
        let emit_req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{{\"name\":\"worktree_lease_emit\",\"arguments\":{{\"worktree_path\":\"{}\",\"task_id\":\"smoke-001\",\"worker\":\"codex\",\"branch\":\"smoke-001\",\"allowed_paths\":[\"src/**\"],\"forbidden_paths\":[\"CLAUDE.md\"],\"test_commands\":[\"cargo test -p x\"]}}}}}}\n",
            wt.display()
        );
        let mut output = Vec::new();
        let server = WorktreeServer::new(PathBuf::from("/repo"));
        serve(emit_req.as_bytes(), &mut output, server).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inner["task_id"], "smoke-001");

        // Now get it back.
        let get_req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"tools/call\",\"params\":{{\"name\":\"worktree_lease_get\",\"arguments\":{{\"worktree_path\":\"{}\"}}}}}}\n",
            wt.display()
        );
        let mut output = Vec::new();
        let server = WorktreeServer::new(PathBuf::from("/repo"));
        serve(get_req.as_bytes(), &mut output, server).unwrap();
        let parsed: Value = serde_json::from_slice(&output).expect("response is JSON");
        let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inner["task_id"], "smoke-001");
        assert_eq!(inner["worker"], "codex");
    }

    #[test]
    fn worktree_lease_check_uses_glob() {
        // Emit lease, then check both an allowed and a forbidden path.
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().to_path_buf();
        let emit_req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":34,\"method\":\"tools/call\",\"params\":{{\"name\":\"worktree_lease_emit\",\"arguments\":{{\"worktree_path\":\"{}\",\"task_id\":\"check-002\",\"worker\":\"codex\",\"branch\":\"check-002\",\"allowed_paths\":[\"src/physics/**\"],\"forbidden_paths\":[\"CLAUDE.md\"],\"test_commands\":[]}}}}}}\n",
            wt.display()
        );
        let mut out = Vec::new();
        let server = WorktreeServer::new(PathBuf::from("/repo"));
        serve(emit_req.as_bytes(), &mut out, server).unwrap();

        for (target, want) in [
            ("src/physics/foo.rs", true),
            ("src/audio/foo.rs", false),
            ("CLAUDE.md", false),
        ] {
            let check_req = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":35,\"method\":\"tools/call\",\"params\":{{\"name\":\"worktree_lease_check\",\"arguments\":{{\"worktree_path\":\"{}\",\"target_path\":\"{}\"}}}}}}\n",
                wt.display(),
                target
            );
            let mut out = Vec::new();
            let server = WorktreeServer::new(PathBuf::from("/repo"));
            serve(check_req.as_bytes(), &mut out, server).unwrap();
            let parsed: Value = serde_json::from_slice(&out).expect("response is JSON");
            let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
            let inner: Value = serde_json::from_str(text).unwrap();
            assert_eq!(inner["allowed"], want, "target={target}");
        }
    }

    #[test]
    fn initialized_notification_returns_no_response() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output, server()).unwrap();
        assert!(
            output.is_empty(),
            "notification produced reply: {:?}",
            output
        );
    }
}
