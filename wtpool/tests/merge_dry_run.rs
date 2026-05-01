//! Integration tests for `merge_to_main` + `cumulative_md`.
//!
//! Two surfaces under test:
//!
//! 1. **Cumulative-md fixtures** (`tests/cumulative_md_fixtures/`):
//!    each case carries `ours.md` + `theirs.md` + `expected.md`. The
//!    driver wraps the two halves with conflict markers, embeds them
//!    in a >=100-line preamble + >=50-line footer (so the
//!    `InProtectedZone` guard does not fire), and asserts the
//!    heuristic produces a body whose post-block lines match
//!    `expected.md` exactly. Per CLAUDE.md Rule 11 this exercises the
//!    algorithm under review, not surrounding glue — deleting the
//!    union-merge logic in `cumulative_md.rs` makes every fixture
//!    test fail. Provenance: synthesized from realistic shapes (see
//!    `cumulative_md_fixtures/README.md`); replace with extracted
//!    real conflicts when the first one lands.
//!
//! 2. **Dry-run end-to-end** (this file's `merge_to_main_dry_run_*`):
//!    builds a tiny fixture repo via `git2`, opens a linked worktree
//!    on a feature branch with one extra commit past main, calls
//!    `merge_to_main` with `dry_run=true`, asserts the proposed
//!    message file exists with the correct trailer + the repo's main
//!    tip is unchanged. No actual merge occurs.

use std::fs;
use std::path::{Path, PathBuf};

use wtpool::cumulative_md::{resolve_cumulative_md_conflict, ConflictKind};
use wtpool::merge::{merge_to_main, MergeRequest, MergeStatus};
use serde_json::Value;
use tempfile::TempDir;

/// Wrap `ours` + `theirs` halves with `<<<<<<<` markers and pad with
/// preamble + footer so the heuristic's `InProtectedZone` reject does
/// not fire (>=100 preamble lines, >=50 footer lines).
fn wrap_conflict(
    ours: &str,
    theirs: &str,
) -> (
    String,
    /* preamble_len */ usize,
    /* footer_len */ usize,
) {
    let preamble_lines = 120;
    let footer_lines = 60;
    let mut s = String::new();
    for i in 0..preamble_lines {
        s.push_str(&format!("preamble line {i}\n"));
    }
    s.push_str("<<<<<<< HEAD\n");
    s.push_str(ours);
    if !ours.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("=======\n");
    s.push_str(theirs);
    if !theirs.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(">>>>>>> branch\n");
    for i in 0..footer_lines {
        s.push_str(&format!("footer line {i}\n"));
    }
    (s, preamble_lines, footer_lines)
}

/// Extract the conflict-block-equivalent slice from the resolved
/// body: drop the same preamble + footer lines we added in
/// [`wrap_conflict`].
fn extract_resolved_block(resolved: &str, preamble_lines: usize, footer_lines: usize) -> String {
    let lines: Vec<&str> = resolved.split_inclusive('\n').collect();
    if lines.len() <= preamble_lines + footer_lines {
        return String::new();
    }
    lines[preamble_lines..lines.len() - footer_lines].concat()
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cumulative_md_fixtures")
}

fn run_fixture(case: &str) {
    let dir = fixture_dir().join(case);
    let ours = fs::read_to_string(dir.join("ours.md")).expect("read ours.md");
    let theirs = fs::read_to_string(dir.join("theirs.md")).expect("read theirs.md");
    let expected = fs::read_to_string(dir.join("expected.md")).expect("read expected.md");

    let (input, preamble_lines, footer_lines) = wrap_conflict(&ours, &theirs);
    let resolved = resolve_cumulative_md_conflict(&input)
        .unwrap_or_else(|e| panic!("case {case}: heuristic bailed: {e:?}"));
    let block = extract_resolved_block(&resolved, preamble_lines, footer_lines);

    assert_eq!(
        block.trim_end(),
        expected.trim_end(),
        "case {case} mismatch:\n--- block ---\n{block}\n--- expected ---\n{expected}"
    );
    // Conflict markers must be entirely gone.
    assert!(
        !resolved.contains("<<<<<<<"),
        "case {case}: leftover marker"
    );
    assert!(
        !resolved.contains("======="),
        "case {case}: leftover marker"
    );
    assert!(
        !resolved.contains(">>>>>>>"),
        "case {case}: leftover marker"
    );
}

#[test]
fn fixture_table_rows_only() {
    run_fixture("case-table-rows-only");
}

#[test]
fn fixture_branch_comments_and_rows() {
    run_fixture("case-branch-comments-and-rows");
}

#[test]
fn fixture_overlapping_row_collapses() {
    run_fixture("case-overlapping-row-collapses");
}

#[test]
fn fixture_three_rows_each_different_branches() {
    run_fixture("case-three-rows-each-different-branches");
}

#[test]
fn fixture_content_mixed_bails_consistently() {
    // Sanity check: a non-table-row line on either side must produce
    // ConflictKind::ContentMixed, regardless of fixture shape. Build
    // adversarial input inline rather than via fixture.
    let (input, _, _) = wrap_conflict(
        "this is prose, not a table row\n",
        "| nav-flowfield-phase2 | shipped |\n",
    );
    let err = resolve_cumulative_md_conflict(&input).unwrap_err();
    assert_eq!(err, ConflictKind::ContentMixed);
}

/// Build a tiny git repo with one commit on `main` and a linked
/// worktree on `feature-x` with one extra commit. Returns the
/// `(tmp, repo_root, wt_path)` triple.
fn fixture_repo_with_branch_worktree() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let sig = git2::Signature::now("t", "t@e.com").unwrap();
    fs::write(root.join("README"), "x").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("README")).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init main", &tree, &[])
        .unwrap();

    // Rename HEAD to `main` if init landed us on `master`.
    if let Ok(head) = repo.head() {
        if head.is_branch() && head.shorthand() == Some("master") {
            repo.branch("main", &head.peel_to_commit().unwrap(), true)
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
    }

    // Create `feature-x` branch + linked worktree with one extra commit.
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch("feature-x", &head_commit, true).unwrap();
    let wt_path = tmp.path().join("wt-feature-x");
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo.worktree("feature-x", &wt_path, Some(&opts)).unwrap();
    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    fs::write(wt_path.join("FEATURE"), "y").unwrap();
    let mut idx = wt_repo.index().unwrap();
    idx.add_path(Path::new("FEATURE")).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = wt_repo.find_tree(tree_oid).unwrap();
    let parent = wt_repo.head().unwrap().peel_to_commit().unwrap();
    wt_repo
        .commit(Some("HEAD"), &sig, &sig, "add FEATURE", &tree, &[&parent])
        .unwrap();

    (tmp, root, wt_path)
}

#[test]
fn merge_to_main_dry_run_writes_proposed_message_no_state_change() {
    let (_tmp, root, wt) = fixture_repo_with_branch_worktree();
    // Snapshot pre-state main tip.
    let pre_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();

    let req = MergeRequest {
        branch: "feature-x".into(),
        reviewer_voices: vec!["torvalds".into(), "lattner".into()],
        merge_message_subject: "feature-x: smoke ship".into(),
        merge_message_body: "Body. Multi-line\nsecond line.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: true,
    };
    let v: Value = merge_to_main(&root, &wt, &req).expect("dry-run ok");
    assert_eq!(v["status"], MergeStatus::DryRun.as_wire());
    let msg_path = v["proposed_message_path"].as_str().expect("path");
    let msg = fs::read_to_string(msg_path).expect("read msg");
    assert!(msg.contains("feature-x: smoke ship"));
    assert!(msg.contains("Body."));
    assert!(msg
        .trim_end()
        .ends_with("Reviewed-by: torvalds\nReviewed-by: lattner"));

    // No state change.
    let post_main = git2::Repository::open(&root)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    assert_eq!(pre_main, post_main, "dry-run must not move main tip");

    // Cleanup the dry-run msg file.
    let _ = fs::remove_file(msg_path);
}

#[test]
fn merge_to_main_rejects_self_merge_voices() {
    let (_tmp, root, wt) = fixture_repo_with_branch_worktree();
    let req = MergeRequest {
        branch: "feature-x".into(),
        reviewer_voices: vec!["worktree-worker".into()],
        // Subject must contain branch substring (Rule 10 guardrail
        // added 2026-04-29) so we exercise the voices check, not the
        // subject check.
        merge_message_subject: "feature-x: voices smoke".into(),
        merge_message_body: "".into(),
        auto_resolve_cumulative_md: true,
        dry_run: true,
    };
    let err = merge_to_main(&root, &wt, &req).unwrap_err();
    assert!(err.contains("reviewer-voice policy"), "got: {err}");
}

#[test]
fn merge_to_main_already_merged_short_circuits() {
    let (_tmp, root, _wt) = fixture_repo_with_branch_worktree();
    // Use main itself as the "branch" — its tip is trivially an
    // ancestor of main, so we hit the already_merged path. We pass
    // root as both repo and worktree; main's branch is `main` so
    // we'll also use that as the branch arg.
    let req = MergeRequest {
        branch: "main".into(),
        reviewer_voices: vec!["torvalds".into()],
        // Subject must contain branch substring per the Rule 10
        // guardrail added 2026-04-29.
        merge_message_subject: "main: already-merged smoke".into(),
        merge_message_body: "".into(),
        auto_resolve_cumulative_md: true,
        dry_run: false,
    };
    let v = merge_to_main(&root, &root, &req).expect("ok");
    assert_eq!(v["status"], MergeStatus::AlreadyMerged.as_wire());
    // No commit was made.
    let pre = v["pre_state"]["main_tip"].as_str().unwrap();
    let post = v["post_state"]["main_tip"].as_str().unwrap();
    assert_eq!(pre, post);
}
