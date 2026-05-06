//! `wtpool` — read-only stdio MCP server + CLI for per-worktree
//! git state and in-flight agent telemetry.
//!
//! Replaces several Bash calls every cascade-merge planning round
//! tends to pay (`git worktree list`, `git -C <wt> log main..HEAD
//! --oneline`, `git -C <wt> status --porcelain`, optionally
//! `git -C <wt> rev-parse HEAD` and `ls /tmp/agent-*` to cross-check
//! live agents) with cached MCP tools.
//!
//! Two surfaces:
//!
//! 1. **CLI** (shell-level debugging):
//!    - `wtpool worktree-list`
//!    - `wtpool worktree-state <path>`
//!    - `wtpool agent-inflight [--stale-minutes N]`
//!    - `wtpool pending-review <branch>`
//!    - `wtpool probe`  (smoke test — see `README.md`)
//!
//! 2. **MCP stdio server** (agent prompt ergonomics):
//!    - `wtpool serve-mcp`

#![deny(missing_docs)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use wtpool::agents::{agent_inflight_summary, DEFAULT_STALE_MINUTES};
use wtpool::compat::repo_root_env;
use wtpool::git::{
    linked_worktree_paths, validate_worktree_path, worktree_list, worktree_state,
};
use wtpool::lease::{LeaseEmitArgs, Worker, WorktreeLease, LEASE_FILENAME};
use wtpool::mcp;
use wtpool::merge::{merge_to_main, MergeRequest};
use wtpool::pool::{pool_acquire, pool_release, pool_status};
use wtpool::reviews::pending_review;
use wtpool::DEFAULT_REPO;

/// Top-level CLI surface.
#[derive(Parser, Debug)]
#[command(
    name = "wtpool",
    version,
    about = "read-only MCP server for worktree + agent telemetry."
)]
struct Cli {
    /// Repository root. Defaults to `WTPOOL_REPO` env var, then
    /// `<REPO_ROOT>`. The repo is opened per call (no long-lived
    /// `git2::Repository` handle), so the same server tracks newly-
    /// added worktrees automatically.
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// CLI subcommands. Every subcommand maps 1:1 to one MCP tool, except
/// `probe` (smoke test) and `serve-mcp` (transport switch).
#[derive(Subcommand, Debug)]
enum Command {
    /// List the workspace's main checkout + every linked worktree.
    WorktreeList,
    /// Per-worktree detail. `path` must be absolute and under
    /// `<REPO_ROOT>`.
    WorktreeState {
        /// Absolute path to the worktree directory.
        path: String,
    },
    /// Cross-reference `/tmp/agent-*` task transcripts and
    /// `/tmp/agent-*.progress` heartbeat sentinels with worktree
    /// paths.
    AgentInflight {
        /// Mtime older than this is reported as stale. Default 5.
        #[arg(long)]
        stale_minutes: Option<u64>,
    },
    /// Stat `/tmp/<branch>-{torvalds,lattner}.md`. Reports first-line
    /// verdict word.
    PendingReview {
        /// Branch name (e.g. `worktree-state-mcp-shim`).
        branch: String,
    },
    /// Smoke test — exercise every tool against the configured repo
    /// and emit a JSON summary. Exit non-zero on any tool failure.
    /// Documented expected output in `README.md`.
    Probe,
    /// MCP stdio server mode — exposes the five tools to a JSON-RPC
    /// 2.0 client over newline-delimited stdio.
    ServeMcp,
    /// Merge a branch into main. CLI mirror of the
    /// `merge_to_main` MCP tool. Refuses self-merge per reviewer-voice policy.
    MergeToMain {
        /// Branch name to merge into main.
        #[arg(long)]
        branch: String,
        /// Reviewer voice for the Reviewed-by trailer. Repeat for
        /// multiple voices. Must include at least one non-
        /// `worktree-worker` voice.
        #[arg(long = "reviewer", value_name = "VOICE")]
        reviewer_voices: Vec<String>,
        /// Subject line of the merge commit (max 72 chars).
        #[arg(long)]
        subject: String,
        /// Body of the merge commit (after subject + blank line).
        #[arg(long, default_value = "")]
        body: String,
        /// Disable the cumulative.md auto-resolve heuristic. Default
        /// is enabled.
        #[arg(long)]
        no_auto_resolve_cumulative_md: bool,
        /// Stop after composing the merge message; no rebase, no
        /// merge.
        #[arg(long)]
        dry_run: bool,
        /// Optional explicit worktree path. Defaults to
        /// `<repo>/wt-pool/<branch>`.
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
    /// Census of pool state — free vs in-use slots under
    /// `<repo>/wt-pool/`.
    PoolStatus,
    /// Claim the first detached+clean pool slot. With `--branch-name`,
    /// create the branch on the slot at `<base_sha>` (default: main
    /// tip). Without `--branch-name`, the slot is returned still
    /// detached at the resolved base sha — caller is responsible for
    /// any subsequent branch operation.
    PoolAcquire {
        /// Optional branch name to create on the acquired slot.
        #[arg(long)]
        branch_name: Option<String>,
        /// Optional base sha; defaults to main's tip.
        #[arg(long)]
        base_sha: Option<String>,
    },
    /// Release a pool slot — detach, hard-reset to main, clean
    /// untracked. Refuses if branch has unmerged commits unless
    /// `--force`.
    PoolRelease {
        /// Absolute path to the pool slot to release.
        #[arg(long)]
        path: PathBuf,
        /// Override the unmerged-commits gate.
        #[arg(long)]
        force: bool,
    },
    /// Worktree lease (Codex /  dispatch contract)
    /// subcommands. See
    /// `tools/wtpool/schemas/worktree-lease.v1.json`.
    Lease {
        #[command(subcommand)]
        action: LeaseAction,
    },
}

/// `lease` subcommand actions.
#[derive(Subcommand, Debug)]
enum LeaseAction {
    /// Emit a fresh lease into `<worktree>/.wt-lease.json`.
    Emit {
        /// Worktree path the lease scopes (must exist).
        #[arg(long)]
        worktree: PathBuf,
        /// Stable task identifier.
        #[arg(long)]
        task_id: String,
        /// Worker tier (claude-opus | claude-sonnet | claude-haiku
        /// | codex | human).
        #[arg(long)]
        worker: String,
        /// Branch the worker commits on.
        #[arg(long)]
        branch: String,
        /// Repeatable allowed-path glob.
        #[arg(long = "allowed", value_name = "GLOB")]
        allowed: Vec<String>,
        /// Repeatable forbidden-path glob (takes precedence over
        /// allowed).
        #[arg(long = "forbidden", value_name = "GLOB")]
        forbidden: Vec<String>,
        /// Repeatable test-command exact-match string.
        #[arg(long = "test", value_name = "CMD")]
        test: Vec<String>,
        /// Override merge authority. Defaults to
        /// `review-agent`.
        #[arg(long)]
        merge_authority: Option<String>,
        /// Optional RFC 3339 soft-expiry stamp.
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// Re-validate an existing lease file. Path defaults to
    /// `<worktree>/.wt-lease.json` when only `--worktree` is
    /// given.
    Validate {
        /// Direct path to the lease JSON. Mutually exclusive with
        /// `--worktree`.
        #[arg(long, conflicts_with = "worktree")]
        path: Option<PathBuf>,
        /// Worktree directory whose `.wt-lease.json` to read.
        #[arg(long, conflicts_with = "path")]
        worktree: Option<PathBuf>,
    },
    /// Check whether a single repo-relative path is permitted by a
    /// lease.
    Check {
        /// Lease JSON path (or use `--worktree` for the canonical
        /// location).
        #[arg(long, conflicts_with = "worktree")]
        path: Option<PathBuf>,
        /// Worktree directory whose `.wt-lease.json` to read.
        #[arg(long, conflicts_with = "path")]
        worktree: Option<PathBuf>,
        /// Repo-relative path to test against the lease's globs.
        #[arg(long)]
        target: String,
        /// Exit non-zero (`2`) when the target is NOT permitted, instead
        /// of always exiting zero with `{"allowed": false}` JSON. Used
        /// by lease-aware hooks (see
        /// `.claude/hooks/lease-path check script`) so a single invocation
        /// both reports + enforces.
        #[arg(long)]
        strict: bool,
    },
    /// Check whether a single bash command exact-matches the lease's
    /// `test_commands` whitelist. Used by the pre-exec hook to
    /// reject any non-whitelisted shell invocation a Codex / cheap-
    /// Claude worker tries to run.
    CheckCmd {
        /// Lease JSON path (or use `--worktree`).
        #[arg(long, conflicts_with = "worktree")]
        path: Option<PathBuf>,
        /// Worktree directory whose `.wt-lease.json` to read.
        #[arg(long, conflicts_with = "path")]
        worktree: Option<PathBuf>,
        /// The literal bash command string to test against the
        /// `test_commands` whitelist (exact-match only).
        #[arg(long)]
        cmd: String,
        /// Exit non-zero (`2`) on mismatch.
        #[arg(long)]
        strict: bool,
    },
    /// Check whether a `cwd` lies inside the lease's `worktree`
    /// subtree, after symlink resolution (TOCTOU mitigation per
    /// feasibility doc §4.3).
    CheckCwd {
        /// Lease JSON path (or use `--worktree`).
        #[arg(long, conflicts_with = "worktree")]
        path: Option<PathBuf>,
        /// Worktree directory whose `.wt-lease.json` to read.
        #[arg(long, conflicts_with = "path")]
        worktree: Option<PathBuf>,
        /// Path to the proposed working directory.
        #[arg(long)]
        cwd: PathBuf,
        /// Exit non-zero (`2`) when `cwd` resolves outside the lease
        /// worktree.
        #[arg(long)]
        strict: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let repo = cli
        .repo
        .clone()
        .or_else(|| repo_root_env().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REPO));

    match cli.command {
        Command::ServeMcp => match mcp::serve_stdio(repo) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": format!("{e}"), "kind": "io"})
                );
                ExitCode::from(1)
            }
        },
        Command::WorktreeList => emit_or_die(worktree_list(&repo)),
        Command::WorktreeState { path } => match validate_worktree_path(&path) {
            Ok(canon) => emit_or_die(worktree_state(&repo, &canon)),
            Err(e) => die(e),
        },
        Command::AgentInflight { stale_minutes } => {
            // Pass real linked-worktree list so the
            // `associate_via_known` fallback can attribute heartbeats
            // whose args/JSONL did not embed a `<repo>/wt-pool/<name>`
            // path.
            let known = linked_worktree_paths(&repo);
            emit_or_die(agent_inflight_summary(
                &repo,
                stale_minutes.unwrap_or(DEFAULT_STALE_MINUTES),
                &known,
            ))
        }
        Command::PendingReview { branch } => emit_or_die(pending_review(&branch)),
        Command::Probe => probe(&repo),
        Command::MergeToMain {
            branch,
            reviewer_voices,
            subject,
            body,
            no_auto_resolve_cumulative_md,
            dry_run,
            worktree,
        } => {
            let req = MergeRequest {
                branch: branch.clone(),
                reviewer_voices,
                merge_message_subject: subject,
                merge_message_body: body,
                auto_resolve_cumulative_md: !no_auto_resolve_cumulative_md,
                dry_run,
            };
            let wt_path = worktree.unwrap_or_else(|| repo.join("wt-pool").join(&branch));
            emit_or_die(merge_to_main(&repo, &wt_path, &req))
        }
        Command::PoolStatus => emit_or_die(pool_status(&repo)),
        Command::PoolAcquire {
            branch_name,
            base_sha,
        } => emit_or_die(pool_acquire(
            &repo,
            branch_name.as_deref(),
            base_sha.as_deref(),
        )),
        Command::PoolRelease { path, force } => emit_or_die(pool_release(&repo, &path, force)),
        Command::Lease { action } => dispatch_lease(action),
    }
}

fn parse_worker(s: &str) -> Result<Worker, String> {
    match s {
        "claude-opus" => Ok(Worker::ClaudeOpus),
        "claude-sonnet" => Ok(Worker::ClaudeSonnet),
        "claude-haiku" => Ok(Worker::ClaudeHaiku),
        "codex" => Ok(Worker::Codex),
        "human" => Ok(Worker::Human),
        other => Err(format!(
            "unknown worker `{other}` (expected one of: claude-opus, claude-sonnet, claude-haiku, codex, human)"
        )),
    }
}

fn dispatch_lease(action: LeaseAction) -> ExitCode {
    match action {
        LeaseAction::Emit {
            worktree,
            task_id,
            worker,
            branch,
            allowed,
            forbidden,
            test,
            merge_authority,
            expires_at,
        } => {
            let worker = match parse_worker(&worker) {
                Ok(w) => w,
                Err(e) => return die(e),
            };
            let args = LeaseEmitArgs {
                task_id,
                worker: Some(worker),
                worktree: worktree.clone(),
                branch,
                allowed_paths: allowed,
                forbidden_paths: forbidden,
                test_commands: test,
                merge_authority,
                expires_at,
                parent_task_id: None,
            };
            let lease = match args.into_lease() {
                Ok(l) => l,
                Err(e) => return die(e.to_string()),
            };
            if let Err(e) = lease.write_to_worktree() {
                return die(e.to_string());
            }
            let lease_path = worktree.join(LEASE_FILENAME);
            emit_or_die(Ok(serde_json::json!({
                "wrote": lease_path.display().to_string(),
                "schema_version": lease.schema_version,
                "task_id": lease.task_id,
            })))
        }
        LeaseAction::Validate { path, worktree } => {
            let lease_path = match resolve_lease_path(path, worktree) {
                Ok(p) => p,
                Err(e) => return die(e),
            };
            match WorktreeLease::read_from_file(&lease_path) {
                Ok(lease) => emit_or_die(Ok(serde_json::json!({
                    "valid": true,
                    "task_id": lease.task_id,
                    "schema_version": lease.schema_version,
                    "path": lease_path.display().to_string(),
                }))),
                Err(e) => die(format!("{}: {e}", e.kind())),
            }
        }
        LeaseAction::Check {
            path,
            worktree,
            target,
            strict,
        } => {
            let lease_path = match resolve_lease_path(path, worktree) {
                Ok(p) => p,
                Err(e) => return die(e),
            };
            match WorktreeLease::read_from_file(&lease_path) {
                Ok(lease) => {
                    let allowed = lease.matches_path(&target);
                    let body = serde_json::json!({
                        "target": target,
                        "allowed": allowed,
                    });
                    println!("{body}");
                    if strict && !allowed {
                        return ExitCode::from(2);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => die(format!("{}: {e}", e.kind())),
            }
        }
        LeaseAction::CheckCmd {
            path,
            worktree,
            cmd,
            strict,
        } => {
            let lease_path = match resolve_lease_path(path, worktree) {
                Ok(p) => p,
                Err(e) => return die(e),
            };
            match WorktreeLease::read_from_file(&lease_path) {
                Ok(lease) => {
                    let allowed = lease.matches_test_command(&cmd);
                    let body = serde_json::json!({
                        "cmd": cmd,
                        "allowed": allowed,
                    });
                    println!("{body}");
                    if strict && !allowed {
                        return ExitCode::from(2);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => die(format!("{}: {e}", e.kind())),
            }
        }
        LeaseAction::CheckCwd {
            path,
            worktree,
            cwd,
            strict,
        } => {
            let lease_path = match resolve_lease_path(path, worktree) {
                Ok(p) => p,
                Err(e) => return die(e),
            };
            match WorktreeLease::read_from_file(&lease_path) {
                Ok(lease) => {
                    let inside = lease.matches_cwd(&cwd);
                    let body = serde_json::json!({
                        "cwd": cwd.display().to_string(),
                        "inside": inside,
                    });
                    println!("{body}");
                    if strict && !inside {
                        return ExitCode::from(2);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => die(format!("{}: {e}", e.kind())),
            }
        }
    }
}

fn resolve_lease_path(path: Option<PathBuf>, worktree: Option<PathBuf>) -> Result<PathBuf, String> {
    match (path, worktree) {
        (Some(p), None) => Ok(p),
        (None, Some(w)) => Ok(w.join(LEASE_FILENAME)),
        (None, None) => Err("must pass --path or --worktree".into()),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
    }
}

fn probe(repo: &std::path::Path) -> ExitCode {
    let mut out = serde_json::Map::new();
    let mut any_err = false;
    for (name, result) in [
        ("worktree_list", worktree_list(repo)),
        (
            "agent_inflight_summary",
            agent_inflight_summary(repo, DEFAULT_STALE_MINUTES, &[]),
        ),
        ("pending_review_smoke", pending_review("_probe_branch_")),
    ] {
        match result {
            Ok(v) => {
                out.insert(name.to_string(), v);
            }
            Err(e) => {
                any_err = true;
                out.insert(name.to_string(), serde_json::json!({ "error": e }));
            }
        }
    }
    let payload = serde_json::Value::Object(out);
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    );
    if any_err {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn emit_or_die(r: Result<serde_json::Value, String>) -> ExitCode {
    match r {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            );
            ExitCode::SUCCESS
        }
        Err(e) => die(e),
    }
}

fn die(e: String) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({"error": e, "kind": "tool_failure"})
    );
    ExitCode::from(1)
}
