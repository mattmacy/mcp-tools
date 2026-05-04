//! `codex_run_task` tool — dispatches one task packet to OpenAI Chat
//! Completions, returning the model's diff + log + token count.
//!
//! ## Arguments
//!
//! - `task_packet` (string, required) — the prompt sent to the model.
//!   The cheap-Claude routing skill is the canonical producer; format
//!   is opaque to this crate.
//! - `worktree_path` (string, required) — absolute path the worker is
//!   intended to operate inside. Validated against
//!   [`crate::WORKTREE_ROOT_PREFIX`] via canonicalize so symlink
//!   escapes fail closed.
//! - `max_tokens` (integer, optional) — output ceiling. Defaults to
//!   131072 (bumped 2026-05-03 from 16384 to give multi-native impl batches headroom).
//! - `model` (string, optional) — overrides
//!   [`crate::DEFAULT_MODEL`]. Allows the routing skill to pick
//!   `gpt-5.5` or `gpt-5.5-pro` for outcome-A-impl shapes while
//!   defaulting to `gpt-5.3-codex` for cheap-tier work.
//!
//! ## Boundary check
//!
//! This is the bring-up baseline. We do exactly one thing here:
//! reject any `worktree_path` that does not canonicalize under
//! the configured worktree-root prefix (default
//! [`crate::WORKTREE_ROOT_PREFIX`], override via
//! `CODEX_STDIO_WORKTREE_ROOT`). That suffices to ensure a typo'd
//! or hostile dispatch cannot point Codex at `/etc` or the main
//! repo checkout. Per-lease allowed_paths/forbidden_paths and
//! post-hoc realpath fencing are out of scope for this crate.

use serde_json::{json, Value};

use crate::codex::{self, ChatMessage, ChatRequest, Client};
use crate::{DEFAULT_MODEL, WORKTREE_ROOT_PREFIX};

/// Resolve the configured worktree-root prefix at call time.
/// Reads `CODEX_STDIO_WORKTREE_ROOT` per call; falls back to
/// [`crate::WORKTREE_ROOT_PREFIX`].
fn worktree_root_prefix() -> String {
    std::env::var("CODEX_STDIO_WORKTREE_ROOT")
        .unwrap_or_else(|_| WORKTREE_ROOT_PREFIX.to_string())
}

/// Default output ceiling when the caller omits `max_tokens`.
pub(crate) const DEFAULT_MAX_TOKENS: u64 = 131_072;

/// Parsed + validated arguments to [`run`]. Intra-crate-only; the
/// integration tests drive the public MCP surface
/// (`mcp::serve`) rather than constructing this struct.
#[derive(Debug)]
pub(crate) struct Args {
    /// Prompt body forwarded to the model.
    pub(crate) task_packet: String,
    /// Worktree path the worker is constrained to. Already
    /// canonicalized + boundary-checked before we hold this struct.
    pub(crate) worktree_path: std::path::PathBuf,
    /// Model name (caller override or default).
    pub(crate) model: String,
    /// Output ceiling.
    pub(crate) max_tokens: u64,
}

/// Validate raw JSON args + run the task. Returns the MCP tool
/// payload as a `Value`.
pub fn run(args: &Value) -> Result<Value, String> {
    let parsed = parse_args(args)?;
    let client = codex::for_worktree(&parsed.worktree_path).map_err(|reason| {
        format!("Codex unavailable: {reason}. Routing skill should fall back to Anthropic tiers.")
    })?;
    dispatch(&parsed, client.as_ref())
}

/// Parse + validate a tools/call arguments object.
pub(crate) fn parse_args(args: &Value) -> Result<Args, String> {
    let task_packet = args
        .get("task_packet")
        .and_then(Value::as_str)
        .ok_or("codex_run_task requires `task_packet` (string)")?
        .to_string();
    if task_packet.is_empty() {
        return Err("codex_run_task `task_packet` must be non-empty".into());
    }
    let worktree_path_raw = args
        .get("worktree_path")
        .and_then(Value::as_str)
        .ok_or("codex_run_task requires `worktree_path` (string)")?;

    let worktree_path = validate_worktree_path(worktree_path_raw)?;

    let model = args
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::var("CODEX_STDIO_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
        });

    let max_tokens = args
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    Ok(Args {
        task_packet,
        worktree_path,
        model,
        max_tokens,
    })
}

/// Canonicalize the worker's worktree path + assert it sits under
/// the pool root. Symlink escapes are caught by `canonicalize`'s
/// realpath resolution; relative paths are rejected up front.
pub(crate) fn validate_worktree_path(raw: &str) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(raw);
    if !p.is_absolute() {
        return Err(format!(
            "worktree_path must be absolute (got `{raw}`); routing skill must pass a fully-resolved path"
        ));
    }
    // canonicalize so a `<root>/<name>/../../etc` path collapses BEFORE
    // the prefix check; otherwise the literal string would pass while
    // the resolved path escapes.
    let canon = std::fs::canonicalize(p)
        .map_err(|e| format!("worktree_path `{raw}` not on disk or unreadable: {e}"))?;
    let prefix = worktree_root_prefix();
    if !canon.starts_with(&prefix) {
        return Err(format!(
            "worktree_path `{raw}` resolves to `{}` which is outside `{}` — Codex worker constrained to the configured worktree root by design",
            canon.display(),
            prefix,
        ));
    }
    Ok(canon)
}

/// Drive a single Chat Completions call + repackage the response.
pub(crate) fn dispatch(args: &Args, client: &dyn Client) -> Result<Value, String> {
    // System message tells the model where it is (the worktree path)
    // and what shape of output we expect (a unified diff). Production
    // would lift this template into a Skill so torvalds can review it
    // alongside the rest of the prompt; today's bring-up uses the
    // minimum necessary for shape conformance.
    let system = format!(
        "You are a Codex worker dispatched by Claude Code's routing skill. Your worktree is `{}`. Produce a unified diff that applies cleanly to that worktree, OR an empty diff with a one-line rationale if the task should be deferred (Outcome B). Do not edit files outside the worktree.",
        args.worktree_path.display(),
    );
    let req = ChatRequest {
        model: args.model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: system,
            },
            ChatMessage {
                role: "user".into(),
                content: args.task_packet.clone(),
            },
        ],
        max_tokens: Some(args.max_tokens),
    };
    // Snapshot worktree HEAD before dispatch so we can later compute the
    // canonical on-disk diff codex actually produced. Under
    // CODEX_STDIO_SANDBOX_BYPASS=1 codex edits files directly via
    // apply_patch; the model's text response is narrative + may wrap
    // the diff in markdown fences that fail `git apply --check`.
    let pre_sha = match git_rev_parse_head(&args.worktree_path) {
        Ok(sha) => Some(sha),
        Err(reason) => {
            log_pre_sha_snapshot_failed(&args.worktree_path, &reason);
            None
        }
    };
    let resp = client.chat(&req)?;
    let choice = resp.choices.first().ok_or("OpenAI returned no choices")?;
    let model_text = choice.message.content.clone();
    // Prefer real on-disk diff over model's narrative text. Falls back
    // to model-text only when worktree is unchanged (Outcome-B honest
    // deferral OR replay-fixture path with no real worktree).
    let diff = match pre_sha
        .as_deref()
        .and_then(|sha| git_diff_from(&args.worktree_path, sha).ok())
    {
        Some(real) if !real.trim().is_empty() => real,
        _ => model_text.clone(),
    };
    // Defense-in-depth: under CODEX_STDIO_SANDBOX_BYPASS=1 codex's vendored
    // bwrap is gone (see env-gate merge a7d17bdf), so a rogue or
    // hallucinating model could emit `apply_patch` headers pointing at
    // absolute paths or `..` traversals outside the worktree. The
    // worktree_path arg is validated up-front via validate_worktree_path,
    // but that doesn't cover what the model produces. Reject any unified-
    // diff header path that escapes the worktree boundary.
    validate_diff_paths_under_worktree(&diff, &args.worktree_path)?;
    let usage = resp.usage.unwrap_or_default();
    // `tokens_used` mirrors OpenAI Chat Completions for stable
    // shape across Anthropic / OpenAI / Codex callers. Codex-only
    // axes (prompt-cache hits, reasoning tokens) ride alongside in
    // `tokens_used_extended` and are `null` on the OpenAI HTTP path
    // until the upstream `usage` block surfaces them. Keeping the
    // axes out of `tokens_used` means the routing skill's existing
    // cost ledger does not need to be re-keyed.
    let (commit_sha, committed_by) =
        decide_commit_outcome(pre_sha.as_deref(), &args.worktree_path)?;
    Ok(json!({
        "diff": diff,
        "commit_sha": commit_sha,
        "committed_by": committed_by,
        "log": format!(
            "model={} finish_reason={} response_id={}",
            resp.model,
            choice.finish_reason.clone().unwrap_or_else(|| "unknown".into()),
            resp.id,
        ),
        "tokens_used": {
            "prompt": usage.prompt_tokens,
            "completion": usage.completion_tokens,
            "total": usage.total_tokens,
        },
        "tokens_used_extended": {
            "cached_prompt": usage.cached_prompt_tokens,
            "reasoning_output": usage.reasoning_output_tokens,
        },
    }))
}

/// Reject any `+++ b/<path>` or `--- a/<path>` header in the model's
/// unified diff whose canonical resolution escapes `worktree`.
///
/// Background: the env-gated sandbox bypass (a7d17bdf) removes codex's
/// vendored bwrap when `CODEX_STDIO_SANDBOX_BYPASS=1`, which means
/// `apply_patch` will trust whatever paths the model emits. The
/// dispatch-level [`validate_worktree_path`] only checks the dispatch
/// argument; this function adds a post-process check on the model's
/// output so a hostile or hallucinating completion cannot edit
/// `/etc/passwd` or `../../sibling-worktree/secrets.json`.
///
/// Behavior:
///
/// - Empty diff (Outcome-B prose-only response) → ok; no headers to
///   validate.
/// - `+++ /dev/null` and `--- /dev/null` sentinels (file-add or
///   file-delete diffs) → skipped.
/// - All other header paths must canonicalize to a path under
///   `worktree.canonicalize()`.
/// - The git-prefix `a/` or `b/` is stripped before resolution.
/// - Absolute paths (starting `/`) are accepted only if they
///   canonicalize under the worktree; this is conservative because the
///   model could emit a worktree-internal absolute path, but it also
///   catches `+++ b//etc/passwd` (note double slash from naive `b/` +
///   `/etc/passwd` concat).
pub(crate) fn validate_diff_paths_under_worktree(
    diff: &str,
    worktree: &std::path::Path,
) -> Result<(), String> {
    // canonicalize once; an unreadable worktree at this point is a bug
    // because validate_worktree_path already canonicalized successfully.
    let worktree_canon = std::fs::canonicalize(worktree).map_err(|e| {
        format!(
            "worktree path `{}` failed to canonicalize for diff validation: {e}",
            worktree.display(),
        )
    })?;
    for line in diff.lines() {
        let raw_path = if let Some(rest) = line.strip_prefix("+++ ") {
            rest
        } else if let Some(rest) = line.strip_prefix("--- ") {
            rest
        } else {
            continue;
        };
        // Header may carry a trailing tab + timestamp (`a/foo.rs\t2026-..`).
        // Real-world unified diffs from `git diff` don't include the
        // timestamp, but POSIX `diff -u` does; strip defensively.
        let path_field = raw_path.split('\t').next().unwrap_or(raw_path).trim();
        if path_field == "/dev/null" {
            continue;
        }
        // Strip git's `a/` or `b/` convention prefix. Note these are
        // NOT path components; they're git's labelling for source/target.
        let stripped = path_field
            .strip_prefix("a/")
            .or_else(|| path_field.strip_prefix("b/"))
            .unwrap_or(path_field);
        // Resolve relative paths against the worktree root; absolute
        // paths are taken as-is (and must still canonicalize under
        // worktree_canon).
        let candidate = if std::path::Path::new(stripped).is_absolute() {
            std::path::PathBuf::from(stripped)
        } else {
            worktree_canon.join(stripped)
        };
        // The path may not exist on disk (new-file diff against an
        // unborn target); canonicalize then falls back to lexical
        // resolution. Use a manual cleanup that collapses `..`
        // components without touching the filesystem when canonicalize
        // fails — but reject any `..` that would escape the worktree.
        let resolved = match std::fs::canonicalize(&candidate) {
            Ok(p) => p,
            Err(_) => lexical_resolve(&candidate),
        };
        if !resolved.starts_with(&worktree_canon) {
            return Err(format!(
                "diff path escapes worktree boundary: `{}` resolves to `{}` which is outside `{}`",
                path_field,
                resolved.display(),
                worktree_canon.display(),
            ));
        }
    }
    Ok(())
}

/// Lexical (filesystem-free) path resolution. Collapses `.` and `..`
/// components without dereferencing symlinks. Used as a fallback when
/// `canonicalize` fails because the diff target doesn't exist yet
/// (new-file diff). Symlink-escape via existing target is still caught
/// by the `canonicalize` Ok-path; this path covers the new-file case
/// only.
fn lexical_resolve(p: &std::path::Path) -> std::path::PathBuf {
    let mut out: Vec<std::path::Component> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                // Only pop if the previous component is a Normal dir;
                // never pop past the root prefix or a leading
                // ParentDir we couldn't resolve.
                let popped = matches!(out.last(), Some(std::path::Component::Normal(_)));
                if popped {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// `git rev-parse HEAD` in the given worktree. Used as the baseline
/// for the on-disk-diff capture in [`dispatch`]. Returns the trimmed
/// 40-char SHA on success; surfaces `git`'s stderr verbatim on failure
/// so misconfigured worktrees fall back to model-text rather than
/// silently swallowing the problem.
pub(crate) fn git_rev_parse_head(worktree: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("spawn git rev-parse: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse HEAD failed in `{}`: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        return Err("git rev-parse HEAD returned empty stdout".into());
    }
    Ok(sha)
}

/// `git status --porcelain` in worktree. Empty output = clean. Used
/// to gate the auto-commit path: codex may have deferred per
/// Outcome-B and emitted prose-only response, in which case
/// committing nothing would create an empty commit.
pub(crate) fn git_status_porcelain(worktree: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("spawn git status: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status --porcelain failed in `{}`: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Decide whether codex committed, did nothing, or left an error state.
///
/// Returns `(Some(sha), "codex")` when HEAD moved, `(None, "none")`
/// when the worktree is clean, and `Err(...)` when codex dirtied the
/// worktree without committing.
pub fn decide_commit_outcome(
    pre_sha: Option<&str>,
    worktree: &std::path::Path,
) -> Result<(Option<String>, &'static str), String> {
    let post_sha = git_rev_parse_head(worktree).ok();
    let codex_committed = match (pre_sha, post_sha.as_deref()) {
        (Some(pre), Some(post)) => pre != post,
        _ => false,
    };
    if codex_committed {
        return Ok((post_sha, "codex"));
    }
    match git_status_porcelain(worktree) {
        Ok(porcelain) if !porcelain.trim().is_empty() => Err(format!(
            "codex-stdio: codex left {} dirty files but did not commit. The shim's manufacture-commit fallback is REFUSED (per feedback_shim_only_commits_when_codex_doesnt structural fix). The orchestrator must instruct codex to run `git add -A && git commit -m <subject>` using the spec's 'Commit message template' section. Files dirty: {}",
            porcelain.lines().count(),
            porcelain.lines().take(5).collect::<Vec<_>>().join(" | "),
        )),
        _ => Ok((None, "none")),
    }
}

fn log_pre_sha_snapshot_failed(worktree: &std::path::Path, reason: &str) {
    log::debug!(
        "[codex-stdio] pre_sha snapshot failed for worktree {}; replay-mode or broken-worktree (model-text capture path active): {}",
        worktree.display(),
        reason,
    );
}

/// `git diff <pre_sha>` in the given worktree. Captures both staged
/// and unstaged changes vs the snapshot, which is what we want — codex
/// under bypass writes via `apply_patch` and may stage or leave
/// unstaged depending on its workflow.
pub(crate) fn git_diff_from(worktree: &std::path::Path, pre_sha: &str) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", pre_sha])
        .output()
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff {pre_sha} failed in `{}`: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    struct TestLogger {
        messages: Mutex<Vec<String>>,
    }

    impl log::Log for TestLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Debug
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                self.messages
                    .lock()
                    .unwrap()
                    .push(format!("{}", record.args()));
            }
        }

        fn flush(&self) {}
    }

    static TEST_LOGGER: TestLogger = TestLogger {
        messages: Mutex::new(Vec::new()),
    };
    static TEST_LOGGER_INIT: Once = Once::new();

    fn init_test_logger() {
        TEST_LOGGER_INIT.call_once(|| {
            let _ = log::set_logger(&TEST_LOGGER);
        });
        log::set_max_level(log::LevelFilter::Debug);
    }

    fn assert_logs_contain<F: FnOnce()>(needle: &str, f: F) {
        init_test_logger();
        TEST_LOGGER.messages.lock().unwrap().clear();
        f();
        let logs = TEST_LOGGER.messages.lock().unwrap().join("\n");
        assert!(
            logs.contains(needle),
            "logs did not contain {needle:?}; got {logs:?}"
        );
    }

    fn init_git_test_worktree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        assert!(init.success());
        for kv in [
            ("user.email", "test@local"),
            ("user.name", "test"),
            ("commit.gpgsign", "false"),
        ] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(wt)
                .args(["config", kv.0, kv.1])
                .status()
                .unwrap();
        }
        std::fs::write(wt.join("seed.txt"), "seed\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["add", "seed.txt"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        tmp
    }

    #[test]
    fn rejects_relative_worktree_path() {
        let err = validate_worktree_path("relative/path").unwrap_err();
        assert!(err.contains("absolute"), "got {err:?}");
    }

    #[test]
    fn rejects_nonexistent_path() {
        let err = validate_worktree_path("/no/such/dir/ever").unwrap_err();
        assert!(err.contains("not on disk"), "got {err:?}");
    }

    /// Mutation-equivalent: removing the `starts_with` boundary check
    /// would let `/etc` through; this asserts the rejection.
    #[test]
    fn rejects_path_outside_worktree_root() {
        let err = validate_worktree_path("/etc").unwrap_err();
        assert!(err.contains("outside"), "got {err:?}");
        assert!(err.contains("/tmp/wtpool/"), "got {err:?}");
    }

    #[test]
    fn parse_args_requires_task_packet() {
        let err = parse_args(&json!({"worktree_path": "/tmp/wtpool/wt-03"}))
            .unwrap_err();
        assert!(err.contains("task_packet"), "got {err:?}");
    }

    #[test]
    fn parse_args_rejects_empty_task_packet() {
        let err = parse_args(&json!({
            "task_packet": "",
            "worktree_path": "/tmp/wtpool/wt-03",
        }))
        .unwrap_err();
        assert!(err.contains("non-empty"), "got {err:?}");
    }

    #[test]
    fn parse_args_uses_default_model_and_max_tokens() {
        // Path must exist on disk for canonicalize to succeed; use
        // this worktree's known root.
        let existing = "/tmp/wtpool/wt-03";
        if !std::path::Path::new(existing).exists() {
            return; // skip when run outside the pool
        }
        let prev_model = std::env::var("CODEX_STDIO_MODEL").ok();
        std::env::remove_var("CODEX_STDIO_MODEL");
        let parsed = parse_args(&json!({
            "task_packet": "fix the null deref in foo.rs:12",
            "worktree_path": existing,
        }))
        .unwrap();
        assert_eq!(parsed.model, DEFAULT_MODEL);
        assert_eq!(parsed.max_tokens, DEFAULT_MAX_TOKENS);
        if let Some(v) = prev_model {
            std::env::set_var("CODEX_STDIO_MODEL", v);
        }
    }

    /// A test client that simulates codex agent mode: side-effects the
    /// worktree (writes a file, runs git add+commit) during chat() so the
    /// shim's post_sha != pre_sha branch is exercised. Mutates the
    /// worktree exactly like CODEX_STDIO_SANDBOX_BYPASS=1 codex would.
    struct CommittingClient {
        worktree: std::path::PathBuf,
        commit_subject: String,
        file_content: String,
    }

    impl crate::codex::Client for CommittingClient {
        fn chat(
            &self,
            _req: &crate::codex::ChatRequest,
        ) -> Result<crate::codex::ChatResponse, String> {
            std::fs::write(self.worktree.join("codex-edit.txt"), &self.file_content)
                .map_err(|e| format!("write codex-edit: {e}"))?;
            let add = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.worktree)
                .args(["add", "codex-edit.txt"])
                .status()
                .map_err(|e| format!("git add: {e}"))?;
            assert!(add.success());
            let commit = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.worktree)
                .args(["commit", "-q", "-m", &self.commit_subject])
                .status()
                .map_err(|e| format!("git commit: {e}"))?;
            assert!(commit.success());
            Ok(crate::codex::ChatResponse {
                id: "chatcmpl-codex-self-commit".into(),
                model: "gpt-5.4".into(),
                choices: vec![crate::codex::Choice {
                    message: crate::codex::ChatMessage {
                        role: "assistant".into(),
                        content: "Committed via bash. See `git log -1`.".into(),
                    },
                    finish_reason: Some("stop".into()),
                }],
                usage: Some(crate::codex::Usage {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    total_tokens: 110,
                    ..Default::default()
                }),
            })
        }
    }

    /// Defer-to-codex-commit: when the model self-commits during the
    /// dispatch (post_sha != pre_sha), the shim must trust that commit
    /// and return its sha as commit_sha + committed_by="codex". The
    /// build_commit_subject manufacture path must NOT fire.
    #[test]
    fn dispatch_uses_codex_self_commit_when_post_sha_diverges() {
        let worktree = init_git_test_worktree();
        let pre_sha = git_rev_parse_head(worktree.path()).expect("pre_sha");
        let client = CommittingClient {
            worktree: worktree.path().to_path_buf(),
            commit_subject: "feat: real codex-shaped commit subject".into(),
            file_content: "codex wrote this\n".into(),
        };
        let args = Args {
            task_packet: "edit + commit".into(),
            worktree_path: worktree.path().to_path_buf(),
            model: "gpt-5.4".into(),
            max_tokens: 4096,
        };
        let v = dispatch(&args, &client).expect("dispatch ok");
        let post_sha = git_rev_parse_head(worktree.path()).expect("post_sha");
        assert_ne!(pre_sha, post_sha, "fixture must move HEAD");
        assert_eq!(
            v["committed_by"].as_str(),
            Some("codex"),
            "expected committed_by=codex; got {:?}",
            v["committed_by"]
        );
        assert_eq!(
            v["commit_sha"].as_str().expect("commit_sha"),
            post_sha,
            "shim must return codex's HEAD as commit_sha"
        );
        // Verify the commit subject is the codex-supplied one, NOT a
        // build_commit_subject manufacture.
        let log_subject = std::process::Command::new("git")
            .arg("-C")
            .arg(worktree.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .expect("git log");
        assert!(log_subject.status.success());
        assert_eq!(
            String::from_utf8_lossy(&log_subject.stdout).trim(),
            "feat: real codex-shaped commit subject"
        );
    }

    /// Mutation-equivalent for the full happy path: substituting the
    /// dispatch with a no-op stub that returns an empty diff would
    /// fail the `assert!(!diff.is_empty())` check.
    #[test]
    fn dispatch_with_replay_client_returns_diff_and_token_count() {
        use crate::codex::ReplayClient;
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let worktree = init_git_test_worktree();
        let body = r#"{
            "id": "chatcmpl-replay-rt",
            "model": "gpt-5.3-codex",
            "choices": [
                {
                    "message": { "role": "assistant", "content": "diff --git a/src/foo.rs b/src/foo.rs\n@@ -10,3 +10,3 @@\n-buggy\n+fixed\n" },
                    "finish_reason": "stop"
                }
            ],
            "usage": { "prompt_tokens": 80, "completion_tokens": 20, "total_tokens": 100 }
        }"#;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        let args = Args {
            task_packet: "fix foo".into(),
            worktree_path: worktree.path().to_path_buf(),
            model: "gpt-5.3-codex".into(),
            max_tokens: 4096,
        };
        let v = dispatch(&args, &client).unwrap();
        let diff = v["diff"].as_str().unwrap();
        assert!(!diff.is_empty(), "diff empty");
        assert!(diff.contains("diff --git"), "missing diff header: {diff:?}");
        assert_eq!(v["tokens_used"]["total"], 100);
        assert_eq!(v["tokens_used"]["prompt"], 80);
        assert_eq!(v["tokens_used"]["completion"], 20);
        // OpenAI replay fixture has no Codex-only axes — extended block
        // must surface the keys with `null` values, not omit them. This
        // pins the response shape so the routing skill's tokens_used
        // ledger can rely on a stable JSON structure across tiers.
        assert!(
            v["tokens_used_extended"].is_object(),
            "tokens_used_extended missing"
        );
        assert!(
            v["tokens_used_extended"]["cached_prompt"].is_null(),
            "cached_prompt should be null on OpenAI replay path: {:?}",
            v["tokens_used_extended"]["cached_prompt"]
        );
        assert!(
            v["tokens_used_extended"]["reasoning_output"].is_null(),
            "reasoning_output should be null on OpenAI replay path: {:?}",
            v["tokens_used_extended"]["reasoning_output"]
        );
        let log = v["log"].as_str().unwrap();
        assert!(
            log.contains("chatcmpl-replay-rt"),
            "log missing id: {log:?}"
        );
        assert!(log.contains("stop"), "log missing finish_reason: {log:?}");
    }

    #[test]
    fn dispatch_emits_extended_axes_when_present_in_usage() {
        // Replay fixture with Codex-only axes set: confirms the
        // run_task layer plumbs cached_prompt / reasoning_output from
        // Usage into tokens_used_extended. The codex.rs Usage struct
        // accepts these via #[serde(default)] so a fixture can carry
        // them; the parse_codex_jsonl unit test in codex.rs covers the
        // upstream side (live `--json` events).
        use crate::codex::ReplayClient;
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let worktree = init_git_test_worktree();
        let body = r#"{
            "id": "chatcmpl-extended",
            "model": "gpt-5.5",
            "choices": [
                { "message": { "role": "assistant", "content": "OK" }, "finish_reason": "stop" }
            ],
            "usage": {
                "prompt_tokens": 13171,
                "completion_tokens": 5,
                "total_tokens": 13176,
                "cached_prompt_tokens": 11648,
                "reasoning_output_tokens": 0
            }
        }"#;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        let args = Args {
            task_packet: "ping".into(),
            worktree_path: worktree.path().to_path_buf(),
            model: "gpt-5.5".into(),
            max_tokens: 64,
        };
        let v = dispatch(&args, &client).unwrap();
        assert_eq!(v["tokens_used_extended"]["cached_prompt"], 11648);
        assert_eq!(v["tokens_used_extended"]["reasoning_output"], 0);
    }

    /// Verifies the on-disk-diff capture path: with a real git checkout,
    /// `git_rev_parse_head` returns the snapshot sha, and after a file
    /// edit `git_diff_from(<snapshot>)` returns a unified diff that
    /// starts with `diff --git`. Mutation: removing the `git diff`
    /// branch in [`dispatch`] (always falling back to model_text) would
    /// not affect this test, but a direct mutation of either helper
    /// (e.g. dropping the `-C <worktree>` arg, returning empty) would.
    #[test]
    fn git_helpers_round_trip_real_worktree() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        // git init + initial commit so HEAD exists.
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        assert!(init.success());
        // Force user identity locally so commit doesn't require global
        // config in CI sandboxes.
        for kv in [
            ("user.email", "test@local"),
            ("user.name", "test"),
            ("commit.gpgsign", "false"),
        ] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(wt)
                .args(["config", kv.0, kv.1])
                .status()
                .unwrap();
        }
        let f = wt.join("hello.txt");
        std::fs::write(&f, "before\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["add", "hello.txt"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        // Snapshot, mutate, diff.
        let pre = git_rev_parse_head(wt).expect("rev-parse HEAD");
        assert_eq!(pre.len(), 40, "want full 40-char sha, got {pre:?}");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f)
            .unwrap()
            .write_all(b"after\n")
            .unwrap();
        let diff = git_diff_from(wt, &pre).expect("git diff");
        assert!(
            diff.starts_with("diff --git"),
            "diff should start with header: {diff:?}",
        );
        assert!(diff.contains("+after"), "diff should show added line");
    }

    /// Mutation-equivalent: if [`git_rev_parse_head`] silently swallowed
    /// failure (e.g. returned `Ok("")` when not in a git checkout), the
    /// dispatch fallback chain in [`dispatch`] would lose its boundary.
    /// This pins the Err path.
    #[test]
    fn git_rev_parse_head_errs_on_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = git_rev_parse_head(tmp.path()).unwrap_err();
        assert!(
            err.contains("rev-parse") || err.contains("git"),
            "got {err:?}"
        );
    }

    #[test]
    fn dispatch_logs_when_pre_sha_snapshot_fails_on_non_git_dir() {
        use crate::codex::ReplayClient;
        use std::io::Write;

        let mut replay = tempfile::NamedTempFile::new().unwrap();
        let body = r#"{
            "id": "chatcmpl-pre-sha-none",
            "model": "gpt-5.3-codex",
            "choices": [
                { "message": { "role": "assistant", "content": "Outcome B: no on-disk diff." }, "finish_reason": "stop" }
            ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        }"#;
        replay.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(replay.path().to_path_buf());
        let worktree = tempfile::tempdir().unwrap();
        let args = Args {
            task_packet: "ping".into(),
            worktree_path: worktree.path().to_path_buf(),
            model: "gpt-5.3-codex".into(),
            max_tokens: 64,
        };

        assert_logs_contain("pre_sha snapshot failed", || {
            dispatch(&args, &client).unwrap();
        });
        assert_logs_contain("replay-mode or broken-worktree", || {
            dispatch(&args, &client).unwrap();
        });
    }

    /// Happy-path: a typical `git diff` output with `a/` + `b/` prefixes
    /// resolves under the worktree.
    #[test]
    fn validate_diff_paths_accepts_paths_inside_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let diff = "diff --git a/src/foo.rs b/src/foo.rs\n--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        validate_diff_paths_under_worktree(diff, tmp.path()).unwrap();
    }

    /// Mutation-equivalent: removing the `starts_with(worktree_canon)`
    /// rejection would let `+++ b//etc/passwd` through. The double
    /// slash is what naive `b/` + `/etc/passwd` concat produces; this
    /// test pins the absolute-path rejection.
    #[test]
    fn validate_diff_paths_rejects_absolute_path_outside_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        // After stripping `b/`, what remains is `/etc/passwd` — an
        // absolute path. The validator must reject because it does not
        // resolve under tmp.
        let diff = "+++ b//etc/passwd\n";
        let err = validate_diff_paths_under_worktree(diff, tmp.path()).unwrap_err();
        assert!(err.contains("escapes worktree boundary"), "got {err:?}");
        assert!(err.contains("/etc/passwd"), "got {err:?}");

        // `--- a/...` form must reject too.
        let diff = "--- a//etc/shadow\n";
        let err = validate_diff_paths_under_worktree(diff, tmp.path()).unwrap_err();
        assert!(err.contains("escapes worktree boundary"), "got {err:?}");
    }

    /// Mutation-equivalent: removing the `lexical_resolve`'s `..`
    /// pop-when-Normal logic, or returning `Ok(())` early in the
    /// validator, would let `+++ b/../sibling/file.rs` through.
    #[test]
    fn validate_diff_paths_rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let diff = "+++ b/../sibling/file.rs\n";
        let err = validate_diff_paths_under_worktree(diff, tmp.path()).unwrap_err();
        assert!(err.contains("escapes worktree boundary"), "got {err:?}");
        assert!(
            err.contains("../sibling/file.rs") || err.contains("sibling"),
            "got {err:?}"
        );
    }

    /// `/dev/null` is git's sentinel for new-file (`--- /dev/null`)
    /// and deleted-file (`+++ /dev/null`) diffs. Must pass through.
    #[test]
    fn validate_diff_paths_accepts_dev_null_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let diff = "diff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+content\n";
        validate_diff_paths_under_worktree(diff, tmp.path()).unwrap();

        let diff = "diff --git a/old.rs b/old.rs\ndeleted file mode 100644\n--- a/old.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-content\n";
        validate_diff_paths_under_worktree(diff, tmp.path()).unwrap();
    }

    /// Outcome-B response shape: model returned plain prose explaining
    /// why no diff was produced. Validator must not invent headers to
    /// reject.
    #[test]
    fn validate_diff_paths_accepts_empty_diff() {
        let tmp = tempfile::tempdir().unwrap();
        validate_diff_paths_under_worktree("", tmp.path()).unwrap();
        validate_diff_paths_under_worktree(
            "I cannot complete this task because the file does not exist.",
            tmp.path(),
        )
        .unwrap();
    }

    /// Mutation-equivalent for the dispatch-level wiring: deleting the
    /// `validate_diff_paths_under_worktree(&diff, &args.worktree_path)?`
    /// call from `dispatch` would let a hostile diff slip through the
    /// JSON-shape happy path, and this test would fail because the
    /// expected `Err` becomes `Ok`. Patterns the existing
    /// `dispatch_with_replay_client_returns_diff_and_token_count`
    /// fixture but loads the message body with a `+++ b//etc/passwd`
    /// header so the validator's loop body actually executes inside
    /// `dispatch`.
    #[test]
    fn dispatch_rejects_hostile_diff_via_validator_wiring() {
        use crate::codex::ReplayClient;
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Embed the hostile `+++ b//etc/passwd` header inside a
        // realistic-shaped unified diff. JSON-escape the newlines so
        // the fixture parses; the model's `content` field becomes the
        // multi-line diff string after deserialization.
        let body = r#"{
            "id": "chatcmpl-replay-hostile",
            "model": "gpt-5.3-codex",
            "choices": [
                {
                    "message": { "role": "assistant", "content": "diff --git a//etc/passwd b//etc/passwd\n--- a//etc/passwd\n+++ b//etc/passwd\n@@ -1,1 +1,1 @@\n-root:x:0:0:root:/root:/bin/bash\n+pwned:x:0:0:root:/root:/bin/bash\n" },
                    "finish_reason": "stop"
                }
            ],
            "usage": { "prompt_tokens": 50, "completion_tokens": 30, "total_tokens": 80 }
        }"#;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        // Use a non-git temp dir so dispatch must validate the model
        // text fixture instead of preferring an unrelated on-disk diff
        // from a shared worktree slot.
        let worktree = tempfile::tempdir().unwrap();
        let args = Args {
            task_packet: "exfiltrate /etc/passwd".into(),
            worktree_path: worktree.path().to_path_buf(),
            model: "gpt-5.3-codex".into(),
            max_tokens: 4096,
        };
        let err = dispatch(&args, &client).unwrap_err();
        assert!(
            err.contains("escapes worktree boundary"),
            "expected validator rejection, got: {err:?}"
        );
        assert!(err.contains("/etc/passwd"), "got: {err:?}");
    }
}
