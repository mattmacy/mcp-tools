//! Integration tests for the reviewer-voice policy carveout (`merge_to_main` skipping
//! reviewer dispatch when every touched path matches the low-blast
//! allowlist).
//!
//! Surfaces under test:
//!
//! - **Allowlist match** — a feature branch that touches only
//!   `benchmark/` + `docs/plans/` + `docs/research/` paths is carveout-
//!   eligible; `merge_to_main` accepts an empty `reviewer_voices` array
//!   and emits `Reviewed-by: rule-13-carveout` in the proposed merge
//!   message (dry-run path).
//! - **One out-of-allowlist file** — same branch with one extra file
//!   under `core/src/` is rejected with the reviewer-voice policy + carveout error.
//! - **Symlink escape** — a symlink under an allowlisted directory that
//!   points outside the worktree (`benchmark/symlink-to-core` →
//!   `core/src/lib.rs`) MUST be ineligible, even though the path's
//!   string form is allowlist-matched. Defends the carveout from name-
//!   only-allowlisting attacks.
//! - **Deletion of allowlisted file** — the deletion-only diff is
//!   carveout-eligible (the path no longer exists in the worktree, so
//!   the symlink check is name-only).
//!
//! All four cases use `dry_run=true` so no merge commit lands; we
//! observe the validation outcome via the proposed-message path's
//! contents (carveout case) or the error message (rejection cases).

use std::fs;
use std::path::{Path, PathBuf};

use wtpool::merge::{merge_to_main, MergeRequest, MergeStatus};
use serde_json::Value;
use tempfile::TempDir;

/// Build a tiny git repo with `main` carrying a baseline file, plus a
/// linked worktree on `feature-carveout` that has applied `mutator` to
/// the worktree's working directory and then committed every change as
/// a single follow-up commit. Returns `(tmp, repo_root, wt_path)`. The
/// fixture seeds `core/src/lib.rs` on `main` so deletion + symlink
/// scenarios have a real target outside the carveout-allowlisted tree.
fn fixture_repo<F: FnOnce(&Path)>(mutator: F) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let sig = git2::Signature::now("t", "t@e.com").unwrap();

    // Seed `main`: README + benchmark/baseline.py + core/src/lib.rs
    // (so deletion + symlink-escape scenarios have a real source file
    // outside the allowlist to play against).
    fs::write(root.join("README"), "x").unwrap();
    fs::create_dir_all(root.join("benchmark")).unwrap();
    fs::write(root.join("benchmark/baseline.py"), "# baseline\n").unwrap();
    fs::create_dir_all(root.join("core/src")).unwrap();
    fs::write(root.join("core/src/lib.rs"), "// load-bearing\n").unwrap();

    let mut idx = repo.index().unwrap();
    for p in ["README", "benchmark/baseline.py", "core/src/lib.rs"] {
        idx.add_path(Path::new(p)).unwrap();
    }
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init main", &tree, &[])
        .unwrap();

    // Rename HEAD to `main` if `master` is the default.
    if let Ok(head) = repo.head() {
        if head.is_branch() && head.shorthand() == Some("master") {
            repo.branch("main", &head.peel_to_commit().unwrap(), true)
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
    }

    // Create `feature-carveout` linked worktree.
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let branch_ref = repo.branch("feature-carveout", &head_commit, true).unwrap();
    let wt_path = tmp.path().join("wt-feature-carveout");
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(branch_ref.get()));
    let _ = repo
        .worktree("feature-carveout", &wt_path, Some(&opts))
        .unwrap();

    // Apply per-test mutations to the working directory, then commit
    // everything as a single new revision on the feature branch via
    // the `git2::Repository` opened against the linked worktree.
    mutator(&wt_path);

    let wt_repo = git2::Repository::open(&wt_path).unwrap();
    let mut idx = wt_repo.index().unwrap();
    // Add all changed paths (untracked + modified) and stage deletions.
    idx.add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    idx.update_all(["*"], None).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = wt_repo.find_tree(tree_oid).unwrap();
    let parent = wt_repo.head().unwrap().peel_to_commit().unwrap();
    wt_repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "feature-carveout: mutator commit",
            &tree,
            &[&parent],
        )
        .unwrap();

    (tmp, root, wt_path)
}

/// Common dry-run merge request shape — every test overrides
/// `reviewer_voices`. Subject contains `feature-carveout` (Rule 10
/// guardrail).
fn req(voices: Vec<String>) -> MergeRequest {
    MergeRequest {
        branch: "feature-carveout".into(),
        reviewer_voices: voices,
        merge_message_subject: "feature-carveout: ship harness-only edits".into(),
        merge_message_body: "Body.".into(),
        auto_resolve_cumulative_md: true,
        dry_run: true,
    }
}

/// Case (a): every touched path matches the allowlist. Empty
/// `reviewer_voices` MUST be accepted; carveout trailer present.
#[test]
fn case_a_all_allowlisted_paths_carveout_eligible() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        // Touch only allowlist-matching paths.
        fs::write(wt.join("benchmark/new.py"), "# bench\n").unwrap();
        fs::create_dir_all(wt.join("docs/plans")).unwrap();
        fs::write(wt.join("docs/plans/x.md"), "# plan\n").unwrap();
        fs::create_dir_all(wt.join("docs/research")).unwrap();
        fs::write(wt.join("docs/research/y.md"), "# research\n").unwrap();
    });

    let v: Value = merge_to_main(&root, &wt, &req(vec![])).expect("carveout dry-run must succeed");
    assert_eq!(v["status"], MergeStatus::DryRun.as_wire());
    let msg_path = v["proposed_message_path"].as_str().expect("path");
    let msg = fs::read_to_string(msg_path).expect("read msg");
    assert!(
        msg.trim_end().ends_with("Reviewed-by: rule-13-carveout"),
        "carveout trailer missing:\n{msg}"
    );
    let _ = fs::remove_file(msg_path);
}

/// Case (b): one path outside the allowlist forces full review.
/// Empty voices MUST be rejected.
#[test]
fn case_b_one_non_allowlisted_path_rejects_empty_voices() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        fs::write(wt.join("benchmark/new.py"), "# bench\n").unwrap();
        // One change OUTSIDE the allowlist.
        fs::write(wt.join("core/src/lib.rs"), "// load-bearing edit\n").unwrap();
    });

    let err = merge_to_main(&root, &wt, &req(vec![])).unwrap_err();
    assert!(
        err.contains("reviewer-voice policy") && err.contains("carveout"),
        "expected reviewer-voice policy + carveout cite, got: {err}"
    );
}

/// Case (c): a symlink under an allowlisted directory pointing OUTSIDE
/// the worktree must NOT carve out, even though the path's string form
/// (`benchmark/symlink-to-core`) is allowlist-matched. Defends the
/// rule from name-only-allowlist attacks.
#[test]
#[cfg(unix)]
fn case_c_symlink_escape_under_allowlisted_dir_rejected() {
    use std::os::unix::fs::symlink;

    // Stage a real target OUTSIDE the worktree to symlink at.
    let outside = TempDir::new().unwrap();
    let outside_target = outside.path().join("payload.rs");
    fs::write(&outside_target, "// outside payload\n").unwrap();

    let (_tmp, root, wt) = fixture_repo(|wt| {
        // Plant a symlink under `benchmark/` (allowlisted) that
        // resolves OUTSIDE the worktree. The path's string form
        // matches `benchmark/**` but the symlink target escapes.
        symlink(&outside_target, wt.join("benchmark/symlink-to-outside")).unwrap();
    });

    let err = merge_to_main(&root, &wt, &req(vec![])).unwrap_err();
    assert!(
        err.contains("reviewer-voice policy") && err.contains("carveout"),
        "symlink-escape must be rejected as ineligible, got: {err}"
    );
}

/// Case (c'): a DANGLING symlink under an allowlisted directory —
/// target does not yet exist on the filesystem at carveout-check time
/// — must also be rejected. Without the
/// `fs::symlink_metadata::is_symlink` guard added post-review
/// 2026-04-29 review, this case slips through (canonicalize errors,
/// falls into the "deletion/rename-from" name-only path, allowlist
/// matches `benchmark/**`, eligible=true). With the guard, dangling
/// symlinks are refused as defense-in-depth: an attacker could plant
/// `/tmp/payload` post-merge and have the build follow the symlink to
/// it.
#[test]
#[cfg(unix)]
fn case_c_dangling_symlink_under_allowlisted_dir_rejected() {
    use std::os::unix::fs::symlink;

    let (_tmp, root, wt) = fixture_repo(|wt| {
        // Plant a symlink whose target does NOT exist anywhere.
        // canonicalize will return Err. Without the symlink_metadata
        // hardening, this slips through to name-only allowlist match.
        symlink(
            "/tmp/rule-13-carveout-payload-does-not-exist",
            wt.join("benchmark/symlink-dangling"),
        )
        .unwrap();
    });

    let err = merge_to_main(&root, &wt, &req(vec![])).unwrap_err();
    assert!(
        err.contains("reviewer-voice policy") && err.contains("carveout"),
        "dangling symlink must be refused as ineligible (defense in depth), got: {err}"
    );
}

/// Case (d): a deletion of an allowlisted file is carveout-eligible.
/// The file no longer exists in the worktree (so the symlink check is
/// name-only), and the deletion's path matches the allowlist.
#[test]
fn case_d_deletion_of_allowlisted_file_carveout_eligible() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        // Remove the seeded benchmark/baseline.py file. `idx.update_all`
        // in the fixture stages the deletion.
        fs::remove_file(wt.join("benchmark/baseline.py")).unwrap();
    });

    let v: Value = merge_to_main(&root, &wt, &req(vec![]))
        .expect("deletion-of-allowlisted-file must be carveout-eligible");
    assert_eq!(v["status"], MergeStatus::DryRun.as_wire());
    let msg_path = v["proposed_message_path"].as_str().expect("path");
    let msg = fs::read_to_string(msg_path).expect("read msg");
    assert!(
        msg.trim_end().ends_with("Reviewed-by: rule-13-carveout"),
        "carveout trailer missing for deletion-only diff:\n{msg}"
    );
    let _ = fs::remove_file(msg_path);
}

/// Dogfood guard: a feature branch that touches the merge.rs
/// implementation itself (mirroring this branch's actual diff) MUST be
/// carveout-INELIGIBLE. Empty voices reject. This pins the
/// "self-test" — if the allowlist ever expands to cover
/// `tools/wtpool/src/`, the cargo-built wtpool
/// could merge itself without review.
#[test]
fn dogfood_branch_touching_merge_impl_is_not_carveout_eligible() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        // Mimic this branch's touch: edit a tools/wtpool
        // source file. The fixture's `core/src/lib.rs` is the most
        // convenient stand-in for "a real source file outside the
        // allowlist"; the underlying check runs on path-string match
        // not file content, so any non-allowlisted path proves the
        // gate.
        fs::create_dir_all(wt.join("tools/wtpool/src")).unwrap();
        fs::write(wt.join("tools/wtpool/src/merge.rs"), "// edit\n").unwrap();
    });

    let err = merge_to_main(&root, &wt, &req(vec![])).unwrap_err();
    assert!(
        err.contains("reviewer-voice policy") && err.contains("carveout"),
        "branch touching merge.rs must require full review, got: {err}"
    );
}


/// Case (e): the three carveout entries added 2026-05-03 — `project/shared/**`,
/// `tools/routing-policy.md`, and `STARTUP.md` — are carveout-eligible. Pre-fix
/// the allowlist held only the original 3 entries, so a doc-only branch
/// touching any of these three drove `merge_to_main` to reject empty
/// `reviewer_voices` and force a full reviewer dispatch even for prose
/// edits with no build surface.
#[test]
fn case_e_2026_05_03_additions_are_carveout_eligible() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        fs::create_dir_all(wt.join("project/shared")).unwrap();
        fs::write(wt.join("project/shared/agent-ledger.md"), "# ledger
").unwrap();
        fs::create_dir_all(wt.join("tools")).unwrap();
        fs::write(wt.join("tools/routing-policy.md"), "# routing
").unwrap();
        fs::write(wt.join("STARTUP.md"), "# startup
").unwrap();
    });

    let v: Value = merge_to_main(&root, &wt, &req(vec![]))
        .expect("2026-05-03 additions must be carveout-eligible");
    assert_eq!(v["status"], MergeStatus::DryRun.as_wire());
    let msg_path = v["proposed_message_path"].as_str().expect("path");
    let msg = fs::read_to_string(msg_path).expect("read msg");
    assert!(
        msg.trim_end().ends_with("Reviewed-by: rule-13-carveout"),
        "carveout trailer missing for 2026-05-03 additions:
{msg}"
    );
    let _ = fs::remove_file(msg_path);
}

/// Case (e'): a sibling file in `tools/` (NOT `tools/routing-policy.md`)
/// MUST NOT be carveout-eligible. Pins the exact-match semantics of the
/// `tools/routing-policy.md` entry against accidental "tools/**" drift.
#[test]
fn case_e_tools_sibling_file_is_not_carveout_eligible() {
    let (_tmp, root, wt) = fixture_repo(|wt| {
        fs::create_dir_all(wt.join("tools")).unwrap();
        fs::write(wt.join("tools/some-other-script.sh"), "# other
").unwrap();
    });

    let err = merge_to_main(&root, &wt, &req(vec![])).unwrap_err();
    assert!(
        err.contains("reviewer-voice policy") && err.contains("carveout"),
        "tools/ sibling must require full review (exact-match guard), got: {err}"
    );
}
