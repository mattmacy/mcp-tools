//! Integration tests for merge-tip enforcement in `merge_to_main`
//! (follow-up tracker row 12).
//!
//! Surface under test: after `git rebase main`, compare each present
//! verdict file's signed `tip:` against the post-rebase branch tip by
//! canonical tree ID (`git rev-parse <sha>^{tree}`), not raw SHA.
//!
//! Cases:
//!
//! - identical-tree SHA shift after rebase proceeds silently
//! - content-different rebase rejects with `merge_tip_drift`
//! - `WTPOOL_ALLOW_TIP_DRIFT=1` overrides the rejection and records it
//! - missing verdict file skips the gate
//! - abbreviated verdict `tip:` resolves via `rev-parse`
//!
//! Mutation probes are run manually per the dispatch spec and recorded
//! in the commit body, not embedded here.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;
use wtpool::merge::{merge_to_main, MergeRequest, MergeStatus};

fn init_repo(root: &Path) -> git2::Repository {
    let repo = git2::Repository::init(root).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "tip-driver").unwrap();
        cfg.set_str("user.email", "tip@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
        cfg.set_str("init.defaultBranch", "main").unwrap();
    }
    repo
}

fn rename_master_to_main(repo: &git2::Repository) {
    if let Ok(head) = repo.head() {
        if head.is_branch() && head.shorthand() == Some("master") {
            let commit = head.peel_to_commit().unwrap();
            repo.branch("main", &commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
            if let Ok(mut master) = repo.find_branch("master", git2::BranchType::Local) {
                let _ = master.delete();
            }
        }
    }
}

fn commit_file(
    repo: &git2::Repository,
    root: &Path,
    name: &str,
    body: &str,
    msg: &str,
) -> git2::Oid {
    fs::write(root.join(name), body).unwrap();
    let sig = git2::Signature::now("tip-driver", "tip@example.test").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(name)).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .unwrap()
}

fn head_sha(root: &Path) -> String {
    git2::Repository::open(root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string()
}

fn extract_verdict_script_for_tests() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/extract-verdict.sh")
}

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_tip_env(verdict_dir: &Path, allow_drift: bool) {
    std::env::set_var("WTPOOL_VERDICT_DIR", verdict_dir);
    std::env::set_var(
        "WTPOOL_EXTRACT_VERDICT_BIN",
        extract_verdict_script_for_tests(),
    );
    if allow_drift {
        std::env::set_var("WTPOOL_ALLOW_TIP_DRIFT", "1");
    } else {
        std::env::remove_var("WTPOOL_ALLOW_TIP_DRIFT");
    }
}

fn unset_tip_env() {
    std::env::remove_var("WTPOOL_VERDICT_DIR");
    std::env::remove_var("WTPOOL_EXTRACT_VERDICT_BIN");
    std::env::remove_var("WTPOOL_ALLOW_TIP_DRIFT");
}

fn parse_envelope(err: &str) -> Value {
    serde_json::from_str(err)
        .unwrap_or_else(|e| panic!("expected JSON envelope, got: {err:?} ({e})"))
}

fn write_verdict(verdict_dir: &Path, branch: &str, voice: &str, tip: &str) -> PathBuf {
    let path = verdict_dir.join(format!("{branch}-{voice}.md"));
    let body = format!(
        "\
---
schema_version: 1
reviewer: {voice}
branch: {branch}
tip: {tip}
base: 1111111
verdict: PROCEED
summary: synthetic test verdict
---

## Summary

Smoke.
"
    );
    fs::write(&path, body).unwrap();
    path
}

fn req(branch: &str) -> MergeRequest {
    MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: format!("{branch}: enforce merge tip"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    }
}

fn fixture_identical_tree_rebase(branch: &str) -> (TempDir, PathBuf, PathBuf, String) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);
    commit_file(&repo, &root, "README", "base\n", "init main");
    rename_master_to_main(&repo);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch(branch, &head_commit, true).unwrap();
    let wt_path = tmp.path().join(format!("wt-{branch}"));
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree(branch, &wt_path, Some(&opts)).unwrap();

    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "tip-driver").unwrap();
        cfg.set_str("user.email", "tip@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }
    commit_file(
        &wt_repo,
        &wt_path,
        "feature-a.txt",
        "feature a\n",
        "branch: add feature-a",
    );
    commit_file(
        &wt_repo,
        &wt_path,
        "feature-b.txt",
        "feature b\n",
        "branch: add feature-b",
    );
    let reviewed_tip = head_sha(&wt_path);

    commit_file(
        &repo,
        &root,
        "feature-a.txt",
        "feature a\n",
        "main: cherry-pick feature-a",
    );

    (tmp, root, wt_path, reviewed_tip)
}

fn fixture_content_drift(branch: &str) -> (TempDir, PathBuf, PathBuf, String) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);
    commit_file(
        &repo,
        &root,
        "common.txt",
        "line 1\nline 2\nline 3\nline 4\n",
        "init main",
    );
    rename_master_to_main(&repo);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch(branch, &head_commit, true).unwrap();
    let wt_path = tmp.path().join(format!("wt-{branch}"));
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree(branch, &wt_path, Some(&opts)).unwrap();

    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "tip-driver").unwrap();
        cfg.set_str("user.email", "tip@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }
    commit_file(
        &wt_repo,
        &wt_path,
        "common.txt",
        "line 1\nline 2\nline 3\nbranch tail\n",
        "branch: edit tail",
    );
    let reviewed_tip = head_sha(&wt_path);

    commit_file(
        &repo,
        &root,
        "common.txt",
        "main head\nline 2\nline 3\nline 4\n",
        "main: edit head",
    );

    (tmp, root, wt_path, reviewed_tip)
}

#[test]
fn identical_tree_rebase_passes() {
    let _lock = env_guard();
    let branch = "tip-identical-tree";
    let (_tmp, root, wt, reviewed_tip) = fixture_identical_tree_rebase(branch);
    let verdict_dir = TempDir::new().unwrap();
    write_verdict(verdict_dir.path(), branch, "torvalds", &reviewed_tip);
    set_tip_env(verdict_dir.path(), false);

    let v = merge_to_main(&root, &wt, &req(branch));
    unset_tip_env();
    let v = v.expect("identical-tree rebase must merge");

    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    assert!(v.get("merge_tip_drift_override").is_none(), "v={v}");
    if let Some(path) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn content_different_rebase_rejected() {
    let _lock = env_guard();
    let branch = "tip-content-drift";
    let (_tmp, root, wt, reviewed_tip) = fixture_content_drift(branch);
    let verdict_dir = TempDir::new().unwrap();
    write_verdict(verdict_dir.path(), branch, "torvalds", &reviewed_tip);
    set_tip_env(verdict_dir.path(), false);
    let pre_main = head_sha(&root);

    let err = merge_to_main(&root, &wt, &req(branch)).unwrap_err();
    unset_tip_env();
    let env = parse_envelope(&err);

    assert_eq!(env["error"], "merge_tip_drift", "envelope={env}");
    assert_eq!(env["reviewed_tip"], reviewed_tip);
    assert_eq!(head_sha(&root), pre_main, "main tip must stay put");
    assert!(
        env["tree_diff_summary"]
            .as_str()
            .unwrap_or_default()
            .contains("common.txt"),
        "envelope={env}"
    );
}

#[test]
fn env_override_allows_drift() {
    let _lock = env_guard();
    let branch = "tip-content-drift-override";
    let (_tmp, root, wt, reviewed_tip) = fixture_content_drift(branch);
    let verdict_dir = TempDir::new().unwrap();
    write_verdict(verdict_dir.path(), branch, "torvalds", &reviewed_tip);
    set_tip_env(verdict_dir.path(), true);

    let v = merge_to_main(&root, &wt, &req(branch));
    unset_tip_env();
    let v = v.expect("override must allow merge");

    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    assert_eq!(
        v["merge_tip_drift_override"]["reviewed_tip"],
        Value::String(reviewed_tip)
    );
    assert!(
        v["merge_tip_drift_override"]["tree_diff_summary"]
            .as_str()
            .unwrap_or_default()
            .contains("common.txt"),
        "v={v}"
    );
    if let Some(path) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn missing_verdict_file_skipped() {
    let _lock = env_guard();
    let branch = "tip-missing-verdict";
    let (_tmp, root, wt, _reviewed_tip) = fixture_content_drift(branch);
    let verdict_dir = TempDir::new().unwrap();
    set_tip_env(verdict_dir.path(), false);

    let v = merge_to_main(&root, &wt, &req(branch));
    unset_tip_env();
    let v = v.expect("missing verdict file should skip tip gate");

    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    assert!(v.get("merge_tip_drift_override").is_none(), "v={v}");
    if let Some(path) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn abbreviated_verdict_tip_resolves() {
    let _lock = env_guard();
    let branch = "tip-abbrev";
    let (_tmp, root, wt, reviewed_tip) = fixture_identical_tree_rebase(branch);
    let verdict_dir = TempDir::new().unwrap();
    let abbrev = reviewed_tip.chars().take(8).collect::<String>();
    write_verdict(verdict_dir.path(), branch, "torvalds", &abbrev);
    set_tip_env(verdict_dir.path(), false);

    let v = merge_to_main(&root, &wt, &req(branch));
    unset_tip_env();
    let v = v.expect("abbreviated verdict tip must resolve");

    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    assert!(v.get("merge_tip_drift_override").is_none(), "v={v}");
    if let Some(path) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(path);
    }
}
