//! Integration tests for `wtpool`.
//!
//! These exercise the public CLI / library surface through the same
//! entrypoints the MCP server uses, against a fixture repo built with
//! `git2`. Per CLAUDE.md Rule 11 each test asserts on observable
//! tool output — deleting a code path the test names should cause the
//! test to fail.
//!
//! What this file covers that `src/*/tests` does not:
//! - End-to-end `serve` loop hitting an MCP `tools/call` request and
//!   parsing the JSON-RPC reply (the in-module tests do this for
//!   error paths; this file does it for the happy path).
//! - `pending_review` cross-checking real `/tmp/<branch>-*.md` files
//!   we own (under a unique-prefix branch name to avoid colliding
//!   with the running session's verdicts).

use std::fs;
use std::path::Path;

use wtpool::mcp::{serve, WorktreeServer};
use wtpool::reviews::{pending_review, verdict_path};
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn pending_review_round_trip_through_real_tmp_files() {
    let prefix = "wtpool-int-test-7c7a3f";
    let branch = format!("{prefix}-pr-rt");
    let torv = verdict_path(&branch, "torvalds");
    let latt = verdict_path(&branch, "lattner");
    // Defensive: scrub any leftover from a prior run.
    let _ = fs::remove_file(&torv);
    let _ = fs::remove_file(&latt);

    fs::write(&torv, "VERDICT: PROCEED\n\nrationale\n").expect("write torv");
    fs::write(&latt, "VERDICT: BOUNCE_BACK\n\ncomments\n").expect("write latt");

    let v = pending_review(&branch).expect("ok");
    assert_eq!(v["torvalds"]["exists"], true);
    assert_eq!(v["torvalds"]["verdict_word"], "proceed");
    assert_eq!(v["lattner"]["exists"], true);
    assert_eq!(v["lattner"]["verdict_word"], "bounce_back");

    fs::remove_file(&torv).ok();
    fs::remove_file(&latt).ok();

    // After cleanup, both must report exists=false.
    let v = pending_review(&branch).expect("ok");
    assert_eq!(v["torvalds"]["exists"], false);
    assert_eq!(v["lattner"]["exists"], false);
}

#[test]
fn mcp_serve_loop_handles_initialize_then_tools_list_then_call() {
    // Pipe three back-to-back JSON-RPC requests; assert three replies
    // on stdout, in order, with matching IDs and well-formed shapes.
    let server = WorktreeServer::new(std::path::PathBuf::from("/repo"));
    let mut input = Vec::new();
    input.extend_from_slice(
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
    );
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n");
    // Use pending_review with a unique-prefix branch so we touch real
    // /tmp without colliding with live verdicts.
    input.extend_from_slice(
        b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"pending_review\",\"arguments\":{\"branch\":\"wtpool-int-test-mcp-loop-zzqq\"}}}\n",
    );
    let mut output = Vec::new();
    serve(input.as_slice(), &mut output, server).expect("serve loop");

    let lines: Vec<&[u8]> = output
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 3, "expected 3 replies, got {}", lines.len());

    let r1: Value = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(r1["id"], 1);
    assert_eq!(r1["result"]["serverInfo"]["name"], "wtpool");

    let r2: Value = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2["id"], 2);
    let tools = r2["result"]["tools"].as_array().expect("tools array");
    // 8 original tools + 3 lease tools (worktree_lease_get/emit/check)
    // landed in worktree-lease-format-spec branch.
    assert_eq!(tools.len(), 11);

    let r3: Value = serde_json::from_slice(lines[2]).unwrap();
    assert_eq!(r3["id"], 3);
    let inner_text = r3["result"]["content"][0]["text"].as_str().expect("text");
    let inner: Value = serde_json::from_str(inner_text).expect("inner JSON");
    assert_eq!(inner["torvalds"]["exists"], false);
    assert_eq!(inner["lattner"]["exists"], false);
}

#[test]
fn worktree_list_against_actual_workspace_returns_main_entry() {
    // Skip if /repo isn't a git repo (e.g. CI runs outside the
    // container). Otherwise assert the response shape is right and
    // includes the main checkout.
    if !Path::new("/tmp/wtpool-repo/.git").exists() {
        return;
    }
    let v =
        wtpool::git::worktree_list(Path::new("/repo")).expect("worktree_list ok");
    let arr = v["worktrees"].as_array().expect("array");
    assert!(!arr.is_empty(), "expected at least main entry");
    assert_eq!(arr[0]["is_main"], serde_json::Value::Bool(true));
    let tip = arr[0]["tip_sha"].as_str().expect("tip_sha string");
    assert_eq!(
        tip.len(),
        40,
        "tip_sha must be full 40-char hex, got {tip:?}"
    );
}

#[test]
fn worktree_state_against_temp_repo_matches_fixture() {
    // Build a tiny fixture repo + assert worktree_state on the main
    // checkout reports zero commits-ahead (since we're querying main
    // itself). Demonstrates the public lib surface end-to-end without
    // needing a linked worktree.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let repo = git2::Repository::init(&root).unwrap();
    let sig = git2::Signature::now("t", "t@e.com").unwrap();
    fs::write(root.join("README"), "x").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("README")).unwrap();
    idx.write().unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let v = wtpool::git::worktree_state(&root, &root).expect("state ok");
    assert_eq!(v["commits_ahead"], 0);
    assert_eq!(v["dirty"], false);
    assert_eq!(v["files_changed"], 0);
}
