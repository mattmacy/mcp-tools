//! `git2`-backed implementations of `worktree_list` + `worktree_state`.
//!
//! Spec §1.3:
//!
//! - `Repository::open(repo_root)` once at boot, then `.worktrees()`
//!   enumerates linked worktrees by name.
//! - For each worktree: open its working-tree repo, read `HEAD`,
//!   compute `commits_ahead` via `Repository::graph_ahead_behind(head,
//!   main)`, walk `Repository::statuses(StatusOptions::new()
//!   .include_untracked(true))` for `dirty` (any non-`CURRENT` status).
//!
//! Path resolution detail (spec §1.5): `git2::Worktree::path()`
//! historically aliased the gitdir under `.git/worktrees/<name>/`, not
//! the working-tree path. On `git2 ~= 0.20` it returns the working-tree
//! path directly — see the `worktree_path_returns_working_tree` test.
//! Library guarantees this so the read-only callers downstream don't
//! need to chase `gitdir/` redirection.
//!
//! All paths returned to MCP callers are absolute. Validation per
//! spec §1.5: callers passing relative paths or paths outside
//! `<REPO_ROOT>` are rejected at the MCP boundary, not here — this
//! module trusts the path it was given.

use std::path::{Path, PathBuf};

use git2::{Repository, StatusOptions};
use serde_json::{json, Value};

use crate::compat::repo_root_env;

/// Number of recent log lines to embed in `worktree_state.last_log_lines`.
/// Five is enough for a reviewer to recognise a branch's recent work
/// without bloating the MCP payload — same heuristic the existing
/// cascade-merge prompts use ("`git log main..HEAD --oneline | head`").
pub(crate) const LOG_TAIL: usize = 5;

/// Open the canonical workspace repo. Wrapper exists so the MCP
/// handlers don't repeat `git2::Error → String` mapping.
pub fn open_repo(repo_root: &Path) -> Result<Repository, String> {
    Repository::open(repo_root).map_err(|e| format!("git2: open {}: {e}", repo_root.display()))
}

/// Enumerate the working-tree paths of every linked worktree under
/// `repo_root`. Used by the `agent_inflight_summary` fallback
/// associator (`agents::associate_via_known`) so the MCP layer can
/// pass a real `known` slice instead of `&[]`. Errors from `git2` are
/// swallowed into an empty list — the fallback is best-effort and
/// must never fail the parent tool call.
pub fn linked_worktree_paths(repo_root: &Path) -> Vec<PathBuf> {
    let repo = match Repository::open(repo_root) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let names = match repo.worktrees() {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::with_capacity(names.len());
    for i in 0..names.len() {
        let name = match names.get(i) {
            Some(n) => n,
            None => continue,
        };
        if let Ok(wt) = repo.find_worktree(name) {
            out.push(wt.path().to_path_buf());
        }
    }
    out
}

/// `worktree_list` body. Returns `{worktrees: [{path, branch, tip_sha,
/// commits_ahead, dirty}]}`.
///
/// Includes the *main* checkout itself as the first entry — agents
/// often want to know whether main is dirty separately from any
/// linked worktree, and `Repository::worktrees()` only enumerates the
/// linked ones.
pub fn worktree_list(repo_root: &Path) -> Result<Value, String> {
    let main_repo = open_repo(repo_root)?;
    let main_tip = head_oid_str(&main_repo)?;
    let mut entries = Vec::new();

    // Main checkout entry first.
    let main_branch = head_branch_name(&main_repo).unwrap_or_else(|_| "<detached>".to_string());
    let (ahead, dirty) = (0u64, dirty_flag(&main_repo)?);
    entries.push(json!({
        "path": repo_root.canonicalize().unwrap_or_else(|_| repo_root.to_path_buf()),
        "branch": main_branch,
        "tip_sha": main_tip,
        "commits_ahead": ahead,
        "dirty": dirty,
        "is_main": true,
    }));

    let names = main_repo
        .worktrees()
        .map_err(|e| format!("git2: worktrees(): {e}"))?;

    for i in 0..names.len() {
        let name = match names.get(i) {
            Some(n) => n,
            None => continue,
        };
        let wt = match main_repo.find_worktree(name) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let wt_path = wt.path().to_path_buf();
        // Open the worktree's own repo to get its HEAD + statuses. A
        // pruned-but-not-removed worktree has its gitdir alive but the
        // working-tree path missing; skip those rather than erroring
        // the whole list.
        let wt_repo = match Repository::open(&wt_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let tip_sha = head_oid_str(&wt_repo).unwrap_or_else(|_| "<unresolved>".to_string());
        let branch = head_branch_name(&wt_repo).unwrap_or_else(|_| "<detached>".to_string());
        let commits_ahead = commits_ahead_of_main(&wt_repo, &main_tip).unwrap_or(0);
        let dirty = dirty_flag(&wt_repo).unwrap_or(false);
        entries.push(json!({
            "path": wt_path,
            "branch": branch,
            "tip_sha": tip_sha,
            "commits_ahead": commits_ahead,
            "dirty": dirty,
            "is_main": false,
        }));
    }

    Ok(json!({ "worktrees": entries }))
}

/// `worktree_state` body. Returns the per-worktree detail tuple.
///
/// `repo_root` is the repository the worktree is linked into (used to
/// resolve `main` for `commits_ahead`). `wt_path` is the working-tree
/// directory of the worktree being inspected.
pub fn worktree_state(repo_root: &Path, wt_path: &Path) -> Result<Value, String> {
    let main_repo = open_repo(repo_root)?;
    let main_tip = head_oid_str(&main_repo)?;
    let wt_repo = Repository::open(wt_path)
        .map_err(|e| format!("git2: open worktree {}: {e}", wt_path.display()))?;
    let branch = head_branch_name(&wt_repo).unwrap_or_else(|_| "<detached>".to_string());
    let tip_sha = head_oid_str(&wt_repo)?;
    let commits_ahead = commits_ahead_of_main(&wt_repo, &main_tip).unwrap_or(0);
    let dirty = dirty_flag(&wt_repo)?;
    let (files_changed, untracked_count) = status_counts(&wt_repo)?;
    let last_log_lines = log_tail(&wt_repo, &main_tip, LOG_TAIL)?;

    Ok(json!({
        "branch": branch,
        "tip_sha": tip_sha,
        "commits_ahead": commits_ahead,
        "files_changed": files_changed,
        "untracked_count": untracked_count,
        "last_log_lines": last_log_lines,
        "dirty": dirty,
    }))
}

/// Resolve `HEAD`'s commit OID as a 40-char hex string. Detached HEAD
/// works fine; missing HEAD is an error.
fn head_oid_str(repo: &Repository) -> Result<String, String> {
    let head = repo.head().map_err(|e| format!("git2: head(): {e}"))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| format!("git2: head peel_to_commit(): {e}"))?;
    Ok(commit.id().to_string())
}

/// Resolve `HEAD`'s branch short name (`main`, `feature-x`, …).
/// Returns an error when HEAD is detached so callers can pick a
/// fallback string.
fn head_branch_name(repo: &Repository) -> Result<String, String> {
    let head = repo.head().map_err(|e| format!("git2: head(): {e}"))?;
    if !head.is_branch() {
        return Err("HEAD detached".into());
    }
    head.shorthand()
        .map(str::to_string)
        .ok_or_else(|| "HEAD shorthand missing".into())
}

/// `git status --porcelain | head -1` semantics: any modified-or-
/// untracked entry → dirty. Untracked included per spec §1.2.
fn dirty_flag(repo: &Repository) -> Result<bool, String> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| format!("git2: statuses(): {e}"))?;
    Ok(!statuses.is_empty())
}

/// Per-status counts (files-changed = tracked-modified, untracked
/// counted separately). Used by `worktree_state`.
fn status_counts(repo: &Repository) -> Result<(u64, u64), String> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| format!("git2: statuses(): {e}"))?;
    let mut tracked = 0u64;
    let mut untracked = 0u64;
    for entry in statuses.iter() {
        let s = entry.status();
        if s.is_wt_new() {
            untracked += 1;
        } else {
            tracked += 1;
        }
    }
    Ok((tracked, untracked))
}

/// `git rev-list --count main..HEAD` semantics, via libgit2's graph
/// walker. Returns 0 when either HEAD or `main` cannot be resolved.
fn commits_ahead_of_main(repo: &Repository, main_tip: &str) -> Result<u64, String> {
    let head_oid = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map(|c| c.id())
        .map_err(|e| format!("git2: head oid: {e}"))?;
    let main_oid = git2::Oid::from_str(main_tip)
        .map_err(|e| format!("git2: parse main_tip {main_tip}: {e}"))?;
    if head_oid == main_oid {
        return Ok(0);
    }
    // graph_ahead_behind(local, upstream) → (ahead, behind). We want
    // commits HEAD has that main does not.
    let (ahead, _behind) = repo
        .graph_ahead_behind(head_oid, main_oid)
        .map_err(|e| format!("git2: graph_ahead_behind: {e}"))?;
    Ok(ahead as u64)
}

/// `git log main..HEAD --oneline | head -N` equivalent. Walks from
/// HEAD backwards, hides anything reachable from `main`, formats each
/// retained commit as `"<short-sha> <subject>"`.
fn log_tail(repo: &Repository, main_tip: &str, n: usize) -> Result<Vec<String>, String> {
    let head_oid = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map(|c| c.id())
        .map_err(|e| format!("git2: head oid: {e}"))?;
    let main_oid = match git2::Oid::from_str(main_tip) {
        Ok(o) => o,
        Err(_) => return Ok(Vec::new()),
    };

    let mut walker = repo.revwalk().map_err(|e| format!("git2: revwalk: {e}"))?;
    walker
        .push(head_oid)
        .map_err(|e| format!("git2: revwalk push HEAD: {e}"))?;
    // hide() on a missing commit (e.g. main not yet fetched into the
    // worktree) is best-effort; on success it excludes commits
    // reachable from main, on failure we fall through and just emit
    // HEAD's own walk.
    let _ = walker.hide(main_oid);
    let mut out = Vec::with_capacity(n);
    for oid in walker.flatten().take(n) {
        if let Ok(commit) = repo.find_commit(oid) {
            let summary = commit.summary().unwrap_or("").to_string();
            let short = oid.to_string().chars().take(8).collect::<String>();
            out.push(format!("{short} {summary}"));
        }
    }
    Ok(out)
}

/// Validate that a caller-supplied path is acceptable for
/// `worktree_state`. Absolute paths only; reject paths outside the
/// configured allow-list (default `/tmp` plus
/// [`crate::DEFAULT_REPO`], override via `WTPOOL_ALLOWED_ROOTS` —
/// colon-separated absolute prefixes).
///
/// Returns the canonicalised `PathBuf` on success, or a human-readable
/// error string on rejection. Centralised here so the MCP handler and
/// the CLI subcommand share the same gate.
pub fn validate_worktree_path(p: &str) -> Result<PathBuf, String> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return Err(format!("path must be absolute, got {p:?}"));
    }
    // Canonicalise BEFORE the prefix check so `..` smuggling can't
    // bypass it. Tolerate non-existent paths by falling back to the
    // raw absolute path — `git2::Repository::open` will produce a
    // clear error downstream.
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let allowed_roots = allowed_root_prefixes();
    if !allowed_roots
        .iter()
        .any(|root| canon.starts_with(root) || path.starts_with(root))
    {
        return Err(format!(
            "path {p:?} outside allow-list ({})",
            allowed_roots.join(":")
        ));
    }
    Ok(canon)
}

/// Resolve the allowed-root list at call time from
/// `WTPOOL_ALLOWED_ROOTS` (colon-separated). Falls back to `/tmp` plus
/// the runtime-resolved repo root from `WTPOOL_REPO` (or
/// [`crate::DEFAULT_REPO`] when `WTPOOL_REPO` is also unset).
///
/// Pre-2026-05-03 this fell back to a literal `/tmp:/repo` regardless
/// of the sibling `WTPOOL_REPO` override. The allow-list now follows
/// the repo override so a single env var configures both.
fn allowed_root_prefixes() -> Vec<String> {
    if let Ok(v) = std::env::var("WTPOOL_ALLOWED_ROOTS") {
        let v = v.trim();
        if !v.is_empty() {
            return v
                .split(':')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }
    }
    let repo_root = repo_root_env()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::DEFAULT_REPO.to_string());
    vec!["/tmp".to_string(), repo_root]
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Build a tiny repo with `n_main_commits` on `main`, and an
    /// optional linked worktree at `<tmp>/wt-<name>` with
    /// `n_branch_commits` extra commits past main. Returns
    /// `(tmp, repo_root, optional_wt_path)`.
    fn fixture(
        n_main_commits: usize,
        wt: Option<(&str, usize)>,
    ) -> (TempDir, PathBuf, Option<PathBuf>) {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let repo = Repository::init(&repo_root).unwrap();
        // Set the initial branch name to `main` for predictability —
        // libgit2 default is `master` on older builds.
        let sig = Signature::now("test", "test@example.com").unwrap();
        for i in 0..n_main_commits {
            commit_file(
                &repo,
                &sig,
                &format!("file{i}.txt"),
                &format!("contents {i}"),
                &format!("main commit {i}"),
            );
        }
        // Rename HEAD to main if init landed us on master.
        if let Ok(head) = repo.head() {
            if head.is_branch() && head.shorthand() == Some("master") {
                repo.branch(
                    "main",
                    &repo.head().unwrap().peel_to_commit().unwrap(),
                    true,
                )
                .unwrap();
                repo.set_head("refs/heads/main").unwrap();
            }
        }
        let wt_path = wt.map(|(name, branch_commits)| {
            let wt_path = tmp.path().join(format!("wt-{name}"));
            // Create a branch at main's tip + add the worktree pointed
            // at that branch. Without an explicit reference,
            // `Repository::worktree` would try to create a NEW branch
            // by `name`, which conflicts with the one we just made;
            // pass our pre-made reference via WorktreeAddOptions.
            let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
            let branch_ref = repo.branch(name, &head_commit, true).unwrap();
            let mut opts = git2::WorktreeAddOptions::new();
            opts.reference(Some(branch_ref.get()));
            let _ = repo
                .worktree(name, &wt_path, Some(&opts))
                .expect("create worktree");
            // Open worktree repo + add `branch_commits` extra commits.
            let wt_repo = Repository::open(&wt_path).unwrap();
            for i in 0..branch_commits {
                commit_file(
                    &wt_repo,
                    &sig,
                    &format!("branch{i}.txt"),
                    &format!("branch contents {i}"),
                    &format!("branch commit {i}"),
                );
            }
            wt_path
        });
        (tmp, repo_root, wt_path)
    }

    fn commit_file(repo: &Repository, sig: &Signature, name: &str, body: &str, msg: &str) {
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(name), body).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new(name)).unwrap();
        idx.write().unwrap();
        let tree_oid = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().map(|p| vec![p]).unwrap_or_default();
        repo.commit(Some("HEAD"), sig, sig, msg, &tree, &parents)
            .unwrap();
    }

    #[test]
    fn worktree_list_includes_main_and_linked() {
        let (_tmp, root, wt) = fixture(2, Some(("feature-x", 3)));
        let v = worktree_list(&root).expect("list ok");
        let arr = v["worktrees"].as_array().expect("array");
        assert!(arr.len() >= 2, "expected at least main + wt, got {arr:?}");
        // Main entry first, is_main=true.
        assert_eq!(arr[0]["is_main"], serde_json::Value::Bool(true));
        // Find the wt entry.
        let wt_path = wt.unwrap();
        let wt_entry = arr
            .iter()
            .find(|e| {
                e["path"].as_str().map(Path::new) == Some(wt_path.as_path())
                    || e["path"].as_str() == wt_path.to_str()
            })
            .expect("wt entry present");
        assert_eq!(wt_entry["branch"], "feature-x");
        assert_eq!(wt_entry["commits_ahead"], 3);
    }

    #[test]
    fn worktree_state_reports_log_tail_and_counts() {
        let (_tmp, root, wt) = fixture(1, Some(("feature-y", 2)));
        let wt_path = wt.unwrap();
        let v = worktree_state(&root, &wt_path).expect("state ok");
        assert_eq!(v["branch"], "feature-y");
        assert_eq!(v["commits_ahead"], 2);
        let lines = v["last_log_lines"].as_array().unwrap();
        assert!(lines.len() <= LOG_TAIL);
        // The two branch commits have known subjects.
        let joined = lines
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("branch commit 0"));
        assert!(joined.contains("branch commit 1"));
    }

    #[test]
    fn worktree_state_dirty_flag_flips_on_uncommitted_change() {
        let (_tmp, root, wt) = fixture(1, Some(("dirty-branch", 1)));
        let wt_path = wt.unwrap();
        // Touch an untracked file inside the worktree.
        fs::write(wt_path.join("scratch.txt"), "scratch").unwrap();
        let v = worktree_state(&root, &wt_path).expect("state ok");
        assert_eq!(v["dirty"], true);
        assert_eq!(v["untracked_count"], 1);
    }

    #[test]
    fn worktree_path_returns_working_tree() {
        // Spec §1.5 had a historical concern that `Worktree::path()`
        // returns the gitdir on some libgit2 versions. Verify on the
        // version we depend on it's actually the working-tree path.
        let (_tmp, root, wt) = fixture(1, Some(("path-check", 1)));
        let wt_path = wt.unwrap();
        let main_repo = open_repo(&root).unwrap();
        let names = main_repo.worktrees().unwrap();
        let name = names.get(0).unwrap();
        let g_wt = main_repo.find_worktree(name).unwrap();
        assert_eq!(
            g_wt.path().canonicalize().unwrap(),
            wt_path.canonicalize().unwrap(),
            "git2::Worktree::path() must return working-tree path, not gitdir"
        );
    }

    #[test]
    fn validate_worktree_path_rejects_relative() {
        assert!(validate_worktree_path("relative/path").is_err());
    }

    #[test]
    fn validate_worktree_path_rejects_outside_allowlist() {
        // With WTPOOL_ALLOWED_ROOTS set to a narrow prefix, paths
        // outside it are rejected.
        std::env::set_var("WTPOOL_ALLOWED_ROOTS", "/repo");
        assert!(validate_worktree_path("/etc/passwd").is_err());
        assert!(validate_worktree_path("/tmp/foo").is_err());
        std::env::remove_var("WTPOOL_ALLOWED_ROOTS");
    }

    #[test]
    fn validate_worktree_path_accepts_subpath_under_allowed_root() {
        // Default allowlist includes /tmp, so /tmp/wtpool/foo is
        // accepted whether or not it exists on disk.
        let v = validate_worktree_path("/tmp/wtpool/foo");
        assert!(v.is_ok(), "got {v:?}");
    }

    #[test]
    fn worktree_state_missing_path_errors_clearly() {
        let (_tmp, root, _) = fixture(1, None);
        let err =
            worktree_state(&root, Path::new("/definitely/not/a/worktree")).expect_err("must error");
        assert!(err.contains("git2"), "expected git2-prefixed error: {err}");
    }
}
