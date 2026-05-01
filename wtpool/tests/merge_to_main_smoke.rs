//! End-to-end smoke for `merge_to_main` RPC.
//!
//! Spec §3.3 describes a 6-step orchestration:
//!
//! 1. Snapshot pre-state (`main_tip`, `branch_tip`).
//! 2. `git rebase main` in the worktree.
//! 3. Cumulative-md-only conflict auto-resolve (when enabled).
//! 4. Compose `Reviewed-by:` trailer.
//! 5. `ALLOW_MAIN_COMMIT=1 git -C <main> merge --no-ff <branch> -F <msg>`.
//! 6. Post-state verify: main_tip moved + commit message contains trailer.
//!
//! `tests/merge_dry_run.rs` exercises step 4 only (dry-run short-
//! circuit). This file exercises every step end-to-end against a
//! `git2`-built fixture repo. Per CLAUDE.md Rule 11, mutation probe:
//! removing `--no-ff` from `merge.rs::merge_to_main` (so the merge
//! becomes a fast-forward when possible) makes
//! [`real_merge_lands_no_ff_commit_with_trailer`] fail because it
//! asserts the new tip has exactly two parents.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use wtpool::merge::{merge_to_main, MergeRequest, MergeStatus};
use serde_json::Value;
use tempfile::TempDir;

/// Configure a fresh `git2::Repository` with a deterministic identity
/// + the `main` default-branch name. Required because `git init` on
/// some platforms still picks `master` and downstream `merge_to_main`
/// hard-codes `main`.
fn init_repo(root: &Path) -> git2::Repository {
    let repo = git2::Repository::init(root).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "smoke-driver").unwrap();
        cfg.set_str("user.email", "smoke@example.test").unwrap();
        // `merge_to_main` does not gpg-sign and would fail under a
        // global gpgsign=true config (e.g. inherited from
        // `~/.gitconfig`). Clamp here so the test is reproducible.
        cfg.set_str("commit.gpgsign", "false").unwrap();
        cfg.set_str("init.defaultBranch", "main").unwrap();
    }
    repo
}

fn commit_file(
    repo: &git2::Repository,
    root: &Path,
    name: &str,
    body: &str,
    msg: &str,
) -> git2::Oid {
    fs::write(root.join(name), body).unwrap();
    let sig = git2::Signature::now("smoke-driver", "smoke@example.test").unwrap();
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

/// Build `<tmp>/repo` with `main` carrying one commit + a linked
/// worktree at `<tmp>/wt-feature-x` on branch `feature-x` carrying one
/// extra commit on top. Returns `(tmp, main_repo_root, worktree_path)`.
fn fixture_clean_branch() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);
    commit_file(&repo, &root, "README", "main r0\n", "init main");

    // `git init` may have left HEAD on `master`; force `main`.
    rename_master_to_main(&repo);

    // Create + check out feature-x in a linked worktree.
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch("feature-x", &head_commit, true).unwrap();
    let wt_path = tmp.path().join("wt-feature-x");
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree("feature-x", &wt_path, Some(&opts)).unwrap();

    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "smoke-driver").unwrap();
        cfg.set_str("user.email", "smoke@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }
    commit_file(&wt_repo, &wt_path, "FEATURE", "feature r0\n", "add FEATURE");

    (tmp, root, wt_path)
}

fn rename_master_to_main(repo: &git2::Repository) {
    if let Ok(head) = repo.head() {
        if head.is_branch() && head.shorthand() == Some("master") {
            let commit = head.peel_to_commit().unwrap();
            repo.branch("main", &commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
            // Drop master ref so subsequent `git rebase main` calls
            // resolve unambiguously.
            if let Ok(mut master) = repo.find_branch("master", git2::BranchType::Local) {
                let _ = master.delete();
            }
        }
    }
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

fn parent_count(root: &Path, sha: &str) -> usize {
    let repo = git2::Repository::open(root).unwrap();
    let oid = git2::Oid::from_str(sha).unwrap();
    let n = repo.find_commit(oid).unwrap().parent_count();
    n
}

fn commit_message(root: &Path, sha: &str) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let oid = git2::Oid::from_str(sha).unwrap();
    let msg = repo
        .find_commit(oid)
        .unwrap()
        .message()
        .unwrap_or_default()
        .to_string();
    msg
}

/// Step 1+5+6: clean rebase, merge commit lands, post-state moves,
/// trailer present, parent_count==2 (`--no-ff` honored).
#[test]
fn real_merge_lands_no_ff_commit_with_trailer() {
    let (_tmp, root, wt) = fixture_clean_branch();
    let pre_main = head_sha(&root);
    let pre_branch = head_sha(&wt);
    assert_ne!(pre_main, pre_branch, "fixture: branch should be ahead");

    let req = MergeRequest {
        branch: "feature-x".into(),
        reviewer_voices: vec!["torvalds".into(), "lattner".into()],
        merge_message_subject: "feature-x: smoke ship".into(),
        merge_message_body: "Smoke driver real-merge body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v: Value = merge_to_main(&root, &wt, &req).expect("merge ok");

    // Step 6 verify: status, post_state shape.
    assert_eq!(v["status"], MergeStatus::Merged.as_wire(), "v={v}");
    let merge_sha = v["merge_sha"].as_str().expect("merge_sha");
    let post_main = v["post_state"]["main_tip"].as_str().expect("post tip");
    assert_eq!(merge_sha, post_main);
    assert_ne!(post_main, pre_main, "main tip must move");

    // Step 1 reflected: pre_state matches snapshot.
    assert_eq!(v["pre_state"]["main_tip"], pre_main);
    assert_eq!(v["pre_state"]["branch_tip"], pre_branch);

    // `--no-ff` invariant: merge commit has exactly 2 parents. This is
    // the mutation probe per CLAUDE.md Rule 11 — drop `--no-ff` from
    // `merge_to_main`'s arg list and the merge becomes fast-forward
    // (parent_count == 1) which fails this assertion.
    assert_eq!(
        parent_count(&root, post_main),
        2,
        "--no-ff must produce 2-parent merge commit"
    );

    // Step 4 trailer present in commit message.
    let msg = commit_message(&root, post_main);
    assert!(msg.contains("feature-x: smoke ship"), "subject: {msg}");
    assert!(
        msg.contains("Reviewed-by: torvalds"),
        "torvalds trailer missing: {msg}"
    );
    assert!(
        msg.contains("Reviewed-by: lattner"),
        "lattner trailer missing: {msg}"
    );
    assert!(msg.contains("Smoke driver real-merge body."));

    // `rendered_command` parity-string honors orchestration spec prefix.
    let rendered = v["rendered_command"].as_str().expect("rendered");
    assert!(
        rendered.starts_with("ALLOW_MAIN_COMMIT=1 "),
        "rendered: {rendered}"
    );
    assert!(rendered.contains("merge --no-ff feature-x -F"));
    assert!(!v["cumulative_md_resolved"].as_bool().unwrap_or(true));

    // Side effect: msg file written to /tmp; clean up.
    let msg_path = v["merge_message_path"].as_str().expect("msg path");
    assert!(Path::new(msg_path).exists());
    let _ = fs::remove_file(msg_path);
}

/// Step 2: rebase conflict outside cumulative.md must abort cleanly +
/// leave main untouched. The `rebase_conflict` payload must list the
/// conflicted file and the `cumulative_md_resolved` flag must be
/// false.
#[test]
fn rebase_conflict_outside_cumulative_md_aborts_main_untouched() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);
    commit_file(&repo, &root, "common.txt", "main r0\n", "init main");
    rename_master_to_main(&repo);

    // Branch off, edit common.txt on feature-x.
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch("feature-x", &head_commit, true).unwrap();
    let wt_path = tmp.path().join("wt-feature-x");
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree("feature-x", &wt_path, Some(&opts)).unwrap();
    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "smoke-driver").unwrap();
        cfg.set_str("user.email", "smoke@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }
    commit_file(
        &wt_repo,
        &wt_path,
        "common.txt",
        "feature edit\n",
        "feature: edit common.txt",
    );

    // Now make a conflicting edit on main.
    commit_file(
        &repo,
        &root,
        "common.txt",
        "main edit\n",
        "main: edit common.txt",
    );

    let pre_main = head_sha(&root);

    let req = MergeRequest {
        branch: "feature-x".into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: "feature-x: should-not-land".into(),
        merge_message_body: "".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v: Value = merge_to_main(&root, &wt_path, &req).expect("call ok");
    assert_eq!(v["status"], MergeStatus::RebaseConflict.as_wire(), "v={v}");

    // Conflict files reported.
    let files = v["rebase_conflicts"].as_array().expect("files");
    assert!(!files.is_empty(), "must report conflicting files");
    let names: Vec<&str> = files.iter().filter_map(|f| f["file"].as_str()).collect();
    assert!(
        names.iter().any(|n| n.ends_with("common.txt")),
        "expected common.txt in {names:?}"
    );
    assert_eq!(v["cumulative_md_resolved"].as_bool().unwrap_or(true), false);

    // Main untouched.
    assert_eq!(head_sha(&root), pre_main, "main tip must not move");

    // Worktree must NOT be in mid-rebase state — `merge_to_main`
    // aborts. Detect by checking `rebase-merge` / `rebase-apply` dirs
    // do not exist.
    let gitdir = Command::new("git")
        .args(["-C", wt_path.to_str().unwrap(), "rev-parse", "--git-dir"])
        .output()
        .unwrap();
    let gitdir = String::from_utf8(gitdir.stdout).unwrap().trim().to_string();
    let gd = if Path::new(&gitdir).is_absolute() {
        PathBuf::from(&gitdir)
    } else {
        wt_path.join(&gitdir)
    };
    assert!(
        !gd.join("rebase-merge").exists(),
        "rebase-merge must be cleaned up"
    );
    assert!(
        !gd.join("rebase-apply").exists(),
        "rebase-apply must be cleaned up"
    );
}

/// Branch ↔ worktree consistency guardrail (added 2026-04-29 after
/// `107974d4`). Construct a worktree on branch `feature-x` then call
/// `merge_to_main(branch="other-branch", worktree_path=<feature-x-wt>)`
/// and assert the orchestrator refuses upfront with a "wrong branch"
/// error. Mutation probe per CLAUDE.md Rule 11: comment out the
/// `worktree_branch != req.branch` check in `merge_to_main` and this
/// test fails (call would otherwise proceed to the already-merged
/// shortcut or rebase, producing a different status).
#[test]
fn rejects_when_worktree_head_branch_does_not_match_req_branch() {
    let (_tmp, root, wt) = fixture_clean_branch();
    let pre_main = head_sha(&root);
    // wt has `feature-x` checked out per fixture_clean_branch, so a
    // request claiming branch `other-branch` is the failure mode.
    let req = MergeRequest {
        branch: "other-branch".into(),
        reviewer_voices: vec!["torvalds".into()],
        // Subject contains branch so subject check passes; we want the
        // worktree-mismatch check to fire.
        merge_message_subject: "other-branch: should-refuse".into(),
        merge_message_body: "".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let err = merge_to_main(&root, &wt, &req).unwrap_err();
    assert!(
        err.contains("wrong branch") && err.contains("Refusing"),
        "expected branch-mismatch refusal, got: {err}"
    );
    // Living evidence: the message must name both the actual checked-
    // out branch and the param.
    assert!(
        err.contains("feature-x"),
        "must name worktree branch: {err}"
    );
    assert!(
        err.contains("other-branch"),
        "must name request branch: {err}"
    );

    // Main untouched: refusal is upfront before any git mutation.
    assert_eq!(head_sha(&root), pre_main, "main tip must not move");
}

/// Step 3: cumulative-doc-only auto-merge path. Produce a synthetic
/// table-row conflict on the cumulative doc, run `merge_to_main` with
/// `auto_resolve_cumulative_md=true`, assert merge lands and
/// `cumulative_md_resolved=true`.
///
/// Validates: the heuristic fires from the rebase code path (not just
/// from the unit tests in `cumulative_md.rs`) and the resolution is
/// staged + `git rebase --continue` proceeds without intervention.
#[test]
fn cumulative_md_auto_resolve_lands_merge_with_flag_set() {
    // Mirrors `merge::CUMULATIVE_MD_REL` (pub(crate), not exposed across
    // the integration-test boundary). Spec §3.5 pins this path; if it
    // ever changes both this constant and the one in `merge.rs` move
    // together.
    const CUMULATIVE_REL: &str = "docs/plans/cumulative.md";
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = init_repo(&root);

    // Build a base cumulative.md with > 100 preamble lines + > 50
    // footer + a `## Status` table both branches will append rows to.
    let mut base = String::new();
    for i in 0..120 {
        base.push_str(&format!("preamble line {i}\n"));
    }
    base.push_str("\n## Status\n\n| branch | state |\n|---|---|\n");
    base.push_str("| base-row | shipped |\n");
    base.push_str("\n## Footer\n\n");
    for i in 0..60 {
        base.push_str(&format!("footer line {i}\n"));
    }

    // Place at the canonical relative path the heuristic targets.
    let cum_abs = root.join(CUMULATIVE_REL);
    fs::create_dir_all(cum_abs.parent().unwrap()).unwrap();
    fs::write(&cum_abs, &base).unwrap();
    let sig = git2::Signature::now("smoke-driver", "smoke@example.test").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(CUMULATIVE_REL)).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init cumulative", &tree, &[])
        .unwrap();
    rename_master_to_main(&repo);

    // Branch + linked worktree.
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch("feature-x", &head_commit, true).unwrap();
    let wt_path = tmp.path().join("wt-feature-x");
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree("feature-x", &wt_path, Some(&opts)).unwrap();
    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    {
        let mut cfg = wt_repo.config().unwrap();
        cfg.set_str("user.name", "smoke-driver").unwrap();
        cfg.set_str("user.email", "smoke@example.test").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
    }

    // On feature-x: append `feature-row` row.
    let wt_cum = wt_path.join(CUMULATIVE_REL);
    let mut feat = base.clone();
    let inject = "| feature-row | shipped |\n";
    feat = feat.replacen(
        "| base-row | shipped |\n",
        &format!("| base-row | shipped |\n{inject}"),
        1,
    );
    fs::write(&wt_cum, &feat).unwrap();
    let mut wt_idx = wt_repo.index().unwrap();
    wt_idx.add_path(Path::new(CUMULATIVE_REL)).unwrap();
    wt_idx.write().unwrap();
    let tree_oid = wt_idx.write_tree().unwrap();
    let tree = wt_repo.find_tree(tree_oid).unwrap();
    let parent = wt_repo.head().unwrap().peel_to_commit().unwrap();
    wt_repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "feature-x: add row",
            &tree,
            &[&parent],
        )
        .unwrap();

    // On main (post-branch-point): append `main-row` row.
    let mut on_main = base.clone();
    let inject_main = "| main-row | shipped |\n";
    on_main = on_main.replacen(
        "| base-row | shipped |\n",
        &format!("| base-row | shipped |\n{inject_main}"),
        1,
    );
    fs::write(&cum_abs, &on_main).unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(CUMULATIVE_REL)).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "main: add row", &tree, &[&parent])
        .unwrap();

    let pre_main = head_sha(&root);

    // Drive merge_to_main with auto-resolve enabled.
    let req = MergeRequest {
        branch: "feature-x".into(),
        reviewer_voices: vec!["torvalds".into()],
        merge_message_subject: "feature-x: cumulative auto-merge smoke".into(),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v: Value = merge_to_main(&root, &wt_path, &req).expect("call ok");
    assert_eq!(
        v["status"],
        MergeStatus::Merged.as_wire(),
        "auto-resolve smoke v={v}"
    );
    assert!(
        v["cumulative_md_resolved"].as_bool().unwrap_or(false),
        "cumulative_md_resolved must be true on auto-resolve path"
    );
    let post_main = v["post_state"]["main_tip"].as_str().unwrap();
    assert_ne!(post_main, pre_main);

    // The merged cumulative.md should contain BOTH rows.
    let merged_body = fs::read_to_string(&cum_abs).unwrap();
    assert!(
        merged_body.contains("| feature-row | shipped |"),
        "missing feature-row"
    );
    assert!(
        merged_body.contains("| main-row | shipped |"),
        "missing main-row"
    );

    // Cleanup msg file.
    if let Some(p) = v["merge_message_path"].as_str() {
        let _ = fs::remove_file(p);
    }
}
