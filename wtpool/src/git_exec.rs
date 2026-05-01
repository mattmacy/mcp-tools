//! Controlled `git` subprocess runner.
//!
//! Used by `merge.rs` to run rebase + merge against the workspace
//! checkout from inside the MCP server. Keeps every shell-out behind
//! one tested entrypoint so the spec §3.3 "leading-env-prefix" detail
//! is honoured uniformly:
//!
//! - The `enforce-main-commit-policy.sh` PreToolUse hook recognises
//!   `ALLOW_MAIN_COMMIT=1` ONLY as a leading env-assignment in the
//!   bash command string — `ALLOW_MAIN_COMMIT=1 git merge …`. Setting
//!   the var via `Command::env()` from a non-Bash-tool subprocess does
//!   not pass through that hook (the hook is a Claude-Code Bash-tool
//!   PreToolUse filter, not a kernel exec hook), so when the merge
//!   command runs from inside this MCP server the hook never fires —
//!   that's by design, the MCP boundary itself enforces the
//!   `reviewer_voices` gate from spec §3.6 / Rule 13.
//! - For interoperability we still spawn with `ALLOW_MAIN_COMMIT=1`
//!   set in the child env, so a future variant that re-shells via
//!   `bash -lc "ALLOW_MAIN_COMMIT=1 git …"` (e.g. for verbose-debug
//!   capture) hits the hook's expected shape.
//!
//! All commands are invoked with an explicit `-C <repo>` so the
//! caller's CWD is irrelevant; the subprocess never inherits a stray
//! `GIT_DIR` / `GIT_WORK_TREE` from the parent.

use std::path::Path;
use std::process::{Command, Stdio};

/// Output of a single `git` subprocess invocation.
#[derive(Debug, Clone)]
pub(crate) struct GitOutput {
    /// Process exit status (`0` on success).
    pub(crate) status: i32,
    /// Captured stdout (may be empty).
    pub(crate) stdout: String,
    /// Captured stderr (may be empty; some `git` subcommands emit
    /// useful progress here even on success).
    pub(crate) stderr: String,
}

impl GitOutput {
    /// Convenience: did the subprocess exit zero?
    pub(crate) fn success(&self) -> bool {
        self.status == 0
    }
}

/// Run `git -C <repo> <args…>` with no special env. Used for
/// every read-only query (status, rev-parse, log, rebase --abort,
/// rebase --continue) that does not require the main-commit override.
pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<GitOutput, String> {
    run_git(repo, args, /*allow_main_commit=*/ false)
}

/// Run `git -C <repo> <args…>` with `ALLOW_MAIN_COMMIT=1` set in the
/// child environment AND prefixed onto the conceptual command shape
/// (recorded in trace logs for parity with the bash-hook regex). Use
/// for `git merge --no-ff` against `main`.
pub(crate) fn git_with_main_commit_override(
    repo: &Path,
    args: &[&str],
) -> Result<GitOutput, String> {
    run_git(repo, args, /*allow_main_commit=*/ true)
}

fn run_git(repo: &Path, args: &[&str], allow_main_commit: bool) -> Result<GitOutput, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    for a in args {
        cmd.arg(a);
    }
    if allow_main_commit {
        cmd.env("ALLOW_MAIN_COMMIT", "1");
    }
    // Always blank GIT_DIR / GIT_WORK_TREE: when the MCP server is
    // launched from inside a worktree, those vars are inherited and
    // would override the explicit `-C <repo>` we just set.
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| {
        format!(
            "git_exec: spawn `git -C {} {:?}` failed: {e}",
            repo.display(),
            args
        )
    })?;
    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

/// Reconstruct the bash-equivalent command string for trace / hook-
/// regex parity. Used by `merge.rs` when emitting `hook_output` so an
/// observer can replay the exact form the
/// `enforce-main-commit-policy.sh` regex would have seen.
///
/// Output shape: `ALLOW_MAIN_COMMIT=1 git -C <repo> <arg1> <arg2> …`
/// (single-quoted args containing whitespace).
pub(crate) fn render_command(repo: &Path, args: &[&str], with_override: bool) -> String {
    let mut s = String::new();
    if with_override {
        s.push_str("ALLOW_MAIN_COMMIT=1 ");
    }
    s.push_str("git -C ");
    s.push_str(&shell_quote(repo.display().to_string().as_str()));
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '/' | '.' | ':' | '=' | '+' | ',' | '@')
    }) {
        return s.into();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn render_command_no_override_no_quote() {
        let s = render_command(Path::new("/repo"), &["status", "--porcelain"], false);
        assert_eq!(s, "git -C /repo status --porcelain");
    }

    #[test]
    fn render_command_with_override_prepends() {
        let s = render_command(
            Path::new("/repo"),
            &["merge", "--no-ff", "feature-x", "-F", "/tmp/msg.txt"],
            true,
        );
        assert!(s.starts_with("ALLOW_MAIN_COMMIT=1 "));
        assert!(s.contains("merge --no-ff feature-x -F /tmp/msg.txt"));
    }

    #[test]
    fn render_command_quotes_whitespace_args() {
        let s = render_command(
            Path::new("/repo"),
            &["commit", "-m", "subject with spaces"],
            false,
        );
        assert!(
            s.contains("'subject with spaces'"),
            "must single-quote whitespace arg: {s}"
        );
    }

    #[test]
    fn render_command_escapes_inner_single_quote() {
        let s = render_command(Path::new("/r"), &["-m", "it's"], false);
        // Standard POSIX trick: close-quote, backslash-quote, reopen.
        assert!(s.contains(r#"'it'\''s'"#), "got {s}");
    }

    #[test]
    fn git_executes_against_real_repo_when_present() {
        // Smoke test against the workspace itself if it exists. Skips
        // gracefully on machines where /repo is not a git repo.
        let repo = PathBuf::from("/repo");
        if !repo.join(".git").exists() {
            return;
        }
        let out = git(&repo, &["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert!(out.success(), "rev-parse failed: {out:?}");
        assert_eq!(out.stdout.trim(), "true");
    }
}
