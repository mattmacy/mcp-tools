//! Integration tests for the lease-compliance gate in
//! `merge_to_main` (closes follow-up-tracker row 6 from
//! `dispatch-review-lease-compliance`).
//!
//! Surface under test: the new `enforce_lease_compliance` step that
//! runs `tools/extract-verdict.sh` against canonical verdict-file paths
//! `/tmp/<branch>-<voice>.md` for each `req.reviewer_voices` entry
//! before rebase + merge. SKILL.md `dispatch-review` §"Lease compliance"
//! promises:
//!
//! - `lease_compliance: clean` + verdict PROCEED → merge proceeds
//! - `lease_compliance: out-of-scope` + verdict PROCEED → merge REJECTED
//!   regardless of verdict word
//! - `lease_compliance: not-applicable` (legacy verdicts without the
//!   field) + PROCEED → merge proceeds (back-compat)
//!
//! Plus two adjacent fail-closed cases:
//!
//! - `extract-verdict.sh` exits non-zero on a probed verdict file →
//!   merge REJECTED with `verdict_parse_failed` (cannot tell whether
//!   it claimed out-of-scope; fail closed).
//! - Multi-voice with one clean + one out-of-scope → REJECTED on the
//!   out-of-scope finding (any single violation aborts the merge).
//!
//! Per CLAUDE.md Rule 11, mutation probe: comment out the call to
//! `enforce_lease_compliance` in `merge_to_main` and the four
//! REJECT cases below all fail (the merges proceed instead of
//! rejecting). The two PROCEED cases also fail in a different mode if
//! the gate is incorrectly broadened to reject `clean` /
//! `not-applicable` verdicts — they assert the merge actually lands.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;
use wtpool::merge::{merge_to_main, MergeRequest, MergeStatus};

/// Configure a fresh `git2::Repository` with a deterministic identity
/// + the `main` default-branch name.
///
/// Mirrors the helper in `tests/merge_to_main_smoke.rs` (kept private
/// per-test-file because integration-test crates do not share modules).
fn init_repo(root: &Path) -> git2::Repository {
    let repo = git2::Repository::init(root).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "lease-driver").unwrap();
        cfg.set_str("user.email", "lease@example.test").unwrap();
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
    let sig = git2::Signature::now("lease-driver", "lease@example.test").unwrap();
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

/// Build a `<tmp>/repo` with `main` carrying one commit + a linked
/// worktree on `branch_name` carrying one extra commit. Caller picks
/// the branch name so per-test verdict-file paths cannot collide
/// across tests in the same crate run.
fn fixture_with_branch(branch_name: &str) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);
    commit_file(&repo, &root, "README", "main r0\n", "init main");
    rename_master_to_main(&repo);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch(branch_name, &head_commit, true).unwrap();
    let wt_path = tmp.path().join(format!("wt-{branch_name}"));
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree(branch_name, &wt_path, Some(&opts)).unwrap();

    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "lease-driver").unwrap();
        cfg.set_str("user.email", "lease@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }
    commit_file(&wt_repo, &wt_path, "FEATURE", "feature r0\n", "add FEATURE");

    (tmp, root, wt_path)
}

/// Materialize a verdict file under `verdict_dir` with the canonical
/// frontmatter `extract-verdict.sh` accepts. `lease_compliance`
/// optional — pass `None` to omit the field entirely (legacy /
/// `not-applicable` path).
fn write_verdict(
    verdict_dir: &Path,
    branch: &str,
    voice: &str,
    tip: &str,
    verdict_word: &str,
    lease_compliance: Option<&str>,
) -> PathBuf {
    let path = verdict_dir.join(format!("{branch}-{voice}.md"));
    let mut body = String::new();
    body.push_str("---\n");
    body.push_str("schema_version: 1\n");
    body.push_str(&format!("reviewer: {voice}\n"));
    body.push_str(&format!("branch: {branch}\n"));
    body.push_str(&format!("tip: {tip}\n"));
    body.push_str("base: 1111111\n");
    body.push_str(&format!("verdict: {verdict_word}\n"));
    body.push_str("summary: synthetic test verdict\n");
    if let Some(lc) = lease_compliance {
        body.push_str(&format!("lease_compliance: {lc}\n"));
    }
    body.push_str("---\n\n## Summary\n\nSmoke.\n");
    fs::write(&path, body).unwrap();
    path
}

/// Resolve `scripts/extract-verdict.sh` next to this crate's manifest.
fn extract_verdict_script_for_tests() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/extract-verdict.sh")
}

/// Process-wide guard for tests that mutate `WTPOOL_VERDICT_DIR` /
/// `WTPOOL_EXTRACT_VERDICT_BIN`. cargo runs integration tests in
/// threads, so concurrent env writes race; the mutex serializes the
/// window during which the gate runs.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

fn set_lease_env(verdict_dir: &Path, script: Option<&Path>) {
    std::env::set_var("WTPOOL_VERDICT_DIR", verdict_dir);
    match script {
        Some(p) => std::env::set_var("WTPOOL_EXTRACT_VERDICT_BIN", p),
        None => std::env::remove_var("WTPOOL_EXTRACT_VERDICT_BIN"),
    }
}

fn unset_lease_env() {
    std::env::remove_var("WTPOOL_VERDICT_DIR");
    std::env::remove_var("WTPOOL_EXTRACT_VERDICT_BIN");
}

fn parse_envelope(err: &str) -> Value {
    serde_json::from_str(err).unwrap_or_else(|e| {
        panic!("expected JSON envelope, got: {err:?} ({e})");
    })
}

// (a) lease_compliance: clean + verdict PROCEED → merge proceeds.
#[test]
fn clean_lease_with_proceed_verdict_lets_merge_land() {
    let _lock = env_guard();
    let branch = "lease-clean-proceed";
    let (_tmp, root, wt) = fixture_with_branch(branch);
    let reviewed_tip = head_sha(&wt);
    let verdict_dir = TempDir::new().unwrap();
    let _vf = write_verdict(
        verdict_dir.path(),
        branch,
        "torvalds",
        &reviewed_tip,
        "PROCEED",
        Some("clean"),
    );

    let script = extract_verdict_script_for_tests();
    set_lease_env(verdict_dir.path(), Some(&script));

    let req = MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: format!("{branch}: clean lease ships"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v = merge_to_main(&root, &wt, &req);
    unset_lease_env();
    let v = v.expect("clean+PROCEED must land");
    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    if let Some(p) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(p);
    }
}

// (b) lease_compliance: out-of-scope + verdict PROCEED → REJECTED with
// lease_violation, regardless of verdict word.
#[test]
fn out_of_scope_lease_with_proceed_rejects_merge() {
    let _lock = env_guard();
    let branch = "lease-oos-proceed";
    let (_tmp, root, wt) = fixture_with_branch(branch);
    let reviewed_tip = head_sha(&wt);
    let verdict_dir = TempDir::new().unwrap();
    let vf = write_verdict(
        verdict_dir.path(),
        branch,
        "torvalds",
        &reviewed_tip,
        "PROCEED",
        Some("out-of-scope"),
    );

    let script = extract_verdict_script_for_tests();
    set_lease_env(verdict_dir.path(), Some(&script));
    let pre_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();

    let req = MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: format!("{branch}: must-be-rejected"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let err = merge_to_main(&root, &wt, &req).unwrap_err();
    unset_lease_env();

    let env = parse_envelope(&err);
    assert_eq!(env["error"], "lease_violation", "envelope={env}");
    assert_eq!(env["voice"], "torvalds");
    assert_eq!(env["verdict_path"], vf.display().to_string());
    assert_eq!(env["details"], "lease_compliance=out-of-scope");

    // Main untouched: rejection is upfront before any git mutation.
    let post_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    assert_eq!(pre_main, post_main, "rejection must not move main tip");
}

// (c) lease_compliance: not-applicable (legacy verdicts without the
// field) + PROCEED → merge proceeds (back-compat). The verdict file
// has NO `lease_compliance:` line; extract-verdict.sh fills the field
// with `not-applicable` in its JSON output.
#[test]
fn legacy_verdict_without_lease_field_lets_merge_land() {
    let _lock = env_guard();
    let branch = "lease-legacy-proceed";
    let (_tmp, root, wt) = fixture_with_branch(branch);
    let reviewed_tip = head_sha(&wt);
    let verdict_dir = TempDir::new().unwrap();
    let _vf = write_verdict(
        verdict_dir.path(),
        branch,
        "torvalds",
        &reviewed_tip,
        "PROCEED",
        None,
    );

    let script = extract_verdict_script_for_tests();
    set_lease_env(verdict_dir.path(), Some(&script));

    let req = MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: format!("{branch}: legacy verdict ok"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v = merge_to_main(&root, &wt, &req);
    unset_lease_env();
    let v = v.expect("not-applicable+PROCEED must land");
    assert_eq!(v["status"], MergeStatus::Merged.as_wire());
    if let Some(p) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(p);
    }
}

// (d) extract-verdict.sh exits non-zero → REJECTED with
// verdict_parse_failed. We force this by writing a malformed verdict
// file (no frontmatter delimiters); the parser exits 2.
#[test]
fn malformed_verdict_file_rejects_with_parse_failed() {
    let _lock = env_guard();
    let branch = "lease-parse-fail";
    let (_tmp, root, wt) = fixture_with_branch(branch);
    let verdict_dir = TempDir::new().unwrap();
    let bad = verdict_dir.path().join(format!("{branch}-torvalds.md"));
    fs::write(&bad, "no frontmatter here\nnot even a delimiter\n").unwrap();

    let script = extract_verdict_script_for_tests();
    set_lease_env(verdict_dir.path(), Some(&script));
    let pre_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();

    let req = MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: format!("{branch}: parser-failure must reject"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let err = merge_to_main(&root, &wt, &req).unwrap_err();
    unset_lease_env();

    let env = parse_envelope(&err);
    assert_eq!(env["error"], "verdict_parse_failed", "envelope={env}");
    assert_eq!(env["voice"], "torvalds");
    assert_eq!(env["verdict_path"], bad.display().to_string());

    let post_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    assert_eq!(pre_main, post_main, "rejection must not move main tip");
}

// (e) Multi-voice: torvalds clean + lattner out-of-scope → merge
// REJECTED. Any single out-of-scope finding aborts; the rejection
// names the offending voice (lattner).
#[test]
fn multi_voice_with_one_out_of_scope_rejects_merge() {
    let _lock = env_guard();
    let branch = "lease-multi-voice";
    let (_tmp, root, wt) = fixture_with_branch(branch);
    let reviewed_tip = head_sha(&wt);
    let verdict_dir = TempDir::new().unwrap();
    let _vf_t = write_verdict(
        verdict_dir.path(),
        branch,
        "torvalds",
        &reviewed_tip,
        "PROCEED",
        Some("clean"),
    );
    let vf_l = write_verdict(
        verdict_dir.path(),
        branch,
        "lattner",
        &reviewed_tip,
        "PROCEED",
        Some("out-of-scope"),
    );

    let script = extract_verdict_script_for_tests();
    set_lease_env(verdict_dir.path(), Some(&script));

    let req = MergeRequest {
        branch: branch.into(),
        reviewer_voices: vec!["torvalds".into(), "lattner".into()],
        merge_message_subject: format!("{branch}: must-be-rejected"),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let err = merge_to_main(&root, &wt, &req).unwrap_err();
    unset_lease_env();

    let env = parse_envelope(&err);
    assert_eq!(env["error"], "lease_violation", "envelope={env}");
    assert_eq!(env["voice"], "lattner");
    assert_eq!(env["verdict_path"], vf_l.display().to_string());
}
