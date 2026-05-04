use codex_stdio::run_task::decide_commit_outcome;

fn run_git(worktree: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {:?} failed", args);
}

fn git_stdout(worktree: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8(out.stdout).expect("utf8 stdout").trim().to_string()
}

fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    run_git(wt, &["init", "-q", "-b", "main"]);
    run_git(wt, &["config", "user.email", "test@local"]);
    run_git(wt, &["config", "user.name", "test"]);
    std::fs::write(wt.join("seed.txt"), "seed\n").expect("write seed");
    run_git(wt, &["add", "seed.txt"]);
    run_git(wt, &["commit", "-q", "-m", "init"]);
    tmp
}

#[test]
fn refuses_when_codex_doesnt_commit_but_worktree_dirty() {
    let repo = init_repo();
    let pre_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("dirty.txt"), "dirty\n").expect("write dirty");

    let err = decide_commit_outcome(Some(pre_sha.as_str()), repo.path()).unwrap_err();
    assert!(err.contains("codex left"), "got {err:?}");
    assert!(
        err.contains("dirty files but did not commit"),
        "got {err:?}"
    );
}

#[test]
fn passes_through_when_codex_commits() {
    let repo = init_repo();
    let pre_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("committed.txt"), "committed\n").expect("write commit");
    run_git(repo.path(), &["add", "committed.txt"]);
    run_git(repo.path(), &["commit", "-q", "-m", "codex committed"]);

    let expected_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    let (commit_sha, committed_by) =
        decide_commit_outcome(Some(pre_sha.as_str()), repo.path()).expect("commit outcome");
    assert_eq!(committed_by, "codex");
    assert_eq!(commit_sha.as_deref(), Some(expected_sha.as_str()));
}

#[test]
fn returns_none_when_no_edits() {
    let repo = init_repo();
    let pre_sha = git_stdout(repo.path(), &["rev-parse", "HEAD"]);

    let (commit_sha, committed_by) =
        decide_commit_outcome(Some(pre_sha.as_str()), repo.path()).expect("commit outcome");
    assert_eq!(committed_by, "none");
    assert!(commit_sha.is_none(), "got {commit_sha:?}");
}
