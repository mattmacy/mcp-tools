//! End-to-end MCP wire-shape tests for `codex-stdio`.
//!
//! Drives the JSON-RPC stdio loop with hand-built request lines and
//! asserts the response payloads. Mirrors `tools/wtpool/
//! tests/integration.rs` shape.
//!
//! These are smoke + boundary tests; deeper unit coverage (env-var
//! ordering, replay-client parse, validate_worktree_path edge cases)
//! lives in `#[cfg(test)] mod tests` blocks alongside the impls.

use std::sync::Mutex;

use serde_json::Value;

use codex_stdio::mcp;

/// Serialise tests that mutate process env. Cargo's default
/// thread-pool would race them; a one-Mutex gate keeps the env
/// consistent without forcing `--test-threads=1` on every CI run.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn drive(line: &[u8]) -> Value {
    let mut output = Vec::new();
    mcp::serve(line, &mut output).expect("mcp::serve");
    serde_json::from_slice(&output).expect("response is valid JSON")
}

#[test]
fn initialize_round_trip_returns_server_info() {
    let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
    let resp = drive(req);
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "codex-stdio");
    assert!(resp["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn tools_list_round_trip_returns_two_codex_tools() {
    let req = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
    let resp = drive(req);
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["codex_health", "codex_run_task"]);
}

/// Smoke test asserts that with the codex binary forced unreachable
/// + no fixture, the `codex_health` tool returns a structured
/// `available: false` payload — never throws an MCP error. Routing
/// skill depends on this graceful-unavailable contract.
#[test]
fn health_round_trip_with_no_codex_binary_returns_unavailable() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
    let prev_bin = std::env::var("CODEX_STDIO_BIN").ok();
    std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE");
    std::env::set_var("CODEX_STDIO_BIN", "/no/such/binary/ever");

    let req =
        b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"codex_health\",\"arguments\":{}}}\n";
    let resp = drive(req);
    assert!(resp.get("error").is_none(), "got error: {:?}", resp);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let inner: Value = serde_json::from_str(text).unwrap();
    assert_eq!(inner["available"], false);
    let reason = inner["reason"].as_str().expect("reason field present");
    assert!(reason.contains("CODEX_STDIO_BIN"), "got reason: {reason:?}");

    if let Some(v) = prev_fixture {
        std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v);
    }
    match prev_bin {
        Some(v) => std::env::set_var("CODEX_STDIO_BIN", v),
        None => std::env::remove_var("CODEX_STDIO_BIN"),
    }
}

/// Full smoke: dispatch `codex_run_task` against the bundled
/// fixture. Validates the wire shape end-to-end without burning
/// tokens or requiring `OPENAI_API_KEY`.
///
/// Mutation-equivalent: commenting out the `from_env` replay-fixture
/// branch would cause the tool to try `from_env` → no key → error.
/// The asserted `diff` content would then be missing.
#[test]
fn run_task_replay_fixture_returns_diff() {
    let _guard = ENV_LOCK.lock().unwrap();
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay/single-line-fix.json");
    let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
    std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", &fixture);

    // Create an existing non-git worktree-shaped directory under the
    // allowed pool root. That keeps this replay test independent of
    // whatever uncommitted diff a shared worktree slot may contain.
    let pool = std::path::Path::new("/tmp/wtpool/pool");
    if !pool.exists() {
        // Outside the pool root — skip rather than fail. Boundary
        // check itself is unit-tested in run_task::tests.
        return;
    }
    let wt = tempfile::Builder::new()
        .prefix("codex-stdio-replay-")
        .tempdir_in(pool)
        .unwrap();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(wt.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(init.success(), "git init failed for replay tempdir");
    let wt = wt.path().display().to_string();

    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\"params\":{{\"name\":\"codex_run_task\",\"arguments\":{{\"task_packet\":\"fix the off-by-one in validate_token\",\"worktree_path\":\"{wt}\"}}}}}}\n"
    );
    let resp = drive(line.as_bytes());
    assert!(resp.get("error").is_none(), "got error: {:?}", resp);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let inner: Value = serde_json::from_str(text).unwrap();
    let diff = inner["diff"].as_str().unwrap();
    assert!(!diff.is_empty(), "diff empty");
    assert!(diff.contains("diff --git"), "missing diff header");
    assert!(diff.contains("auth.rs"), "missing expected file");
    assert_eq!(inner["tokens_used"]["total"], 229);
    let log = inner["log"].as_str().unwrap();
    assert!(log.contains("chatcmpl-replay-single-line-fix"));

    match prev_fixture {
        Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
        None => std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE"),
    }
}

/// Mutation-equivalent: removing the `validate_worktree_path`
/// boundary check (or relaxing the `starts_with` constraint) would
/// let `/etc` through and the assertion below would fail.
#[test]
fn run_task_rejects_worktree_outside_pool() {
    let _guard = ENV_LOCK.lock().unwrap();
    // Even with a valid replay fixture, the boundary check must
    // run before any HTTP/replay call.
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay/single-line-fix.json");
    let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
    std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", &fixture);

    let line = b"{\"jsonrpc\":\"2.0\",\"id\":43,\"method\":\"tools/call\",\"params\":{\"name\":\"codex_run_task\",\"arguments\":{\"task_packet\":\"hostile\",\"worktree_path\":\"/etc\"}}}\n";
    let resp = drive(line);
    assert!(resp.get("error").is_some(), "expected error, got {resp:?}");
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("outside"), "got msg: {msg:?}");
    assert!(
        msg.contains("/tmp/wtpool/"),
        "got msg: {msg:?}"
    );

    match prev_fixture {
        Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
        None => std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE"),
    }
}

#[test]
fn run_task_missing_required_args_returns_invalid_params_or_internal() {
    // Bad request: missing both required args.
    let line = b"{\"jsonrpc\":\"2.0\",\"id\":44,\"method\":\"tools/call\",\"params\":{\"name\":\"codex_run_task\",\"arguments\":{}}}\n";
    let resp = drive(line);
    assert!(resp.get("error").is_some());
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("task_packet"), "got msg: {msg:?}");
}

#[test]
fn replay_event_stream_fixture_exposes_usage_axes() {
    // Loads the committed exec-event-stream.jsonl fixture via the public
    // env-var contract. Asserts dispatch returns tokens_used_extended
    // with cached_prompt_tokens and reasoning_output_tokens populated —
    // the whole point of the JSONL refactor.
    let _guard = ENV_LOCK.lock().unwrap();
    let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
    let prev_root = std::env::var("CODEX_STDIO_WORKTREE_ROOT").ok();
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay/exec-event-stream.jsonl");
    std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", &fixture);

    // Override the worktree-root prefix so the test can use a path
    // under the cargo target dir without depending on `/tmp/wtpool/`
    // existing on the host.
    let target_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    let mut prefix = target_dir.display().to_string();
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    std::env::set_var("CODEX_STDIO_WORKTREE_ROOT", &prefix);

    let worktree_path = target_dir.join("replay-event-stream-worktree");
    std::fs::create_dir_all(&worktree_path).unwrap();
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":45,\"method\":\"tools/call\",\"params\":{{\"name\":\"codex_run_task\",\"arguments\":{{\"task_packet\":\"fix the off-by-one in validate_token\",\"worktree_path\":\"{}\"}}}}}}\n",
        worktree_path.display()
    );
    let result = drive(line.as_bytes());
    match prev_fixture {
        Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
        None => std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE"),
    }
    match prev_root {
        Some(v) => std::env::set_var("CODEX_STDIO_WORKTREE_ROOT", v),
        None => std::env::remove_var("CODEX_STDIO_WORKTREE_ROOT"),
    }
    assert!(result.get("error").is_none(), "got error: {:?}", result);
    let text = result["result"]["content"][0]["text"].as_str().unwrap();
    let resp: Value = serde_json::from_str(text).unwrap();
    assert!(resp["diff"].as_str().unwrap().contains("diff --git"));
    let ext = &resp["tokens_used_extended"];
    assert_eq!(ext["cached_prompt"], 960);
    assert_eq!(ext["reasoning_output"], 12);
}
