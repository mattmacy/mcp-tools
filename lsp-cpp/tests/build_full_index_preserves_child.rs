//! Integration test: `build_full_index` MUST NOT terminate the wrapped
//! clangd subprocess.
//!
//! v0.2 had a lifetime bug: `Clangd::build_full_index` took ownership
//! of `self.stdout` for a detached drain thread, and to let the thread
//! exit on EOF it sent `exit` + dropped stdin + SIGKILLed the child at
//! the end of every call. The next `workspace_symbol` over MCP saw a
//! reaped clangd, the supervisor span a fresh child via
//! `ensure_spawned()`, and the new child ran with an empty in-memory
//! symbol table — turning the entire pre-warm run into wasted wall
//! clock.
//!
//! The fix in v0.3 replaces the detached drain thread with an inline
//! `libc::poll`-driven drain so `self.stdout` stays owned by the
//! `Clangd` struct and the child is never killed at end of
//! `build_full_index`. This test is the deletion-guard: it spawns the
//! shim in `serve-mcp` mode against a tiny generated fixture project,
//! captures `clangd_pid` before and after a `build_full_index` call,
//! and asserts the PID is unchanged.
//!
//! It also verifies the `restart_count` is unchanged (no supervisor
//! cycle) and that a follow-up `workspace_symbol` lands on a hit
//! defined inside the fixture — proving the in-memory symbol table
//! the call is supposed to populate is actually populated and queryable
//! through the same long-lived clangd process.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// Per-attempt sleep between workspace_symbol retries while clangd
/// finishes parsing the fixture TU. The fixture is one tiny C++ file
/// so this should resolve in well under a second on a hot toolchain.
const RETRY_SLEEP_MS: u64 = 1000;
/// Hard ceiling on how long we wait for the fixture symbol to appear.
const RETRY_TOTAL_MS: u64 = 60_000;

fn shim_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lsp-cpp"))
}

/// Skip when no clangd-19 is on PATH. The test is meaningful only on
/// hosts where the wrapper can actually spawn a real clangd; CI hosts
/// without llvm installed get a `skipped` line instead of a spurious
/// failure.
fn clangd_available() -> bool {
    let bin = std::env::var("CLANGD_BIN").unwrap_or_else(|_| "clangd-19".to_string());
    Command::new(&bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a tiny C++ fixture project under `<tempdir>/proj` with one
/// translation unit defining a uniquely-named struct, plus a
/// `compile_commands.json` that points at it. Returns the project root.
///
/// The struct name is intentionally unusual (`UniqueFixtureWidget_v03`)
/// so the post-build_full_index `workspace_symbol` hit can't be
/// satisfied by a stale system header that happened to be in the
/// index — it has to come from the TU we just opened.
fn build_fixture(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let src = root.join("widget.cpp");
    std::fs::write(
        &src,
        "struct UniqueFixtureWidget_v03 { int channel; };\n\
         int fixture_entry() { UniqueFixtureWidget_v03 w{0}; return w.channel; }\n",
    )?;
    // compile_commands.json — single TU, no external includes so a
    // missing system header path can't poison the parse.
    let cdb = format!(
        r#"[{{
            "directory": "{}",
            "command": "clang++ -std=c++17 -c widget.cpp",
            "file": "{}"
        }}]"#,
        root.display(),
        src.display(),
    );
    std::fs::write(root.join("compile_commands.json"), cdb)?;
    Ok(())
}

/// Spawn the shim in `serve-mcp` mode pointed at `project_root` and
/// drive the MCP `initialize` handshake. Returns the child plus its
/// captured stdio so the test body can issue further `tools/call`
/// requests.
fn spawn_shim(project_root: &Path) -> (Child, ChildStdin, BufReader<std::process::ChildStdout>) {
    let bin = shim_binary();
    let mut child = Command::new(&bin)
        .arg("serve-mcp")
        .env("LSP_PROJECT", project_root)
        .env("LSP_CPP_INDEX_MODE", "narrow")
        // Keep the shim quiet about seed-didOpen — we want the only
        // `didOpen` activity in this test to come from build_full_index
        // itself, not from a baked seed list.
        .env("LSP_CPP_SEED_HEADERS", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn lsp-cpp serve-mcp");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    (child, stdin, stdout)
}

/// Send `initialize` + `notifications/initialized` over the MCP stdio
/// transport (newline-delimited JSON-RPC, NOT LSP-framed).
fn handshake(stdin: &mut ChildStdin, stdout: &mut BufReader<std::process::ChildStdout>) {
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read initialize reply");
    let init: Value = serde_json::from_str(line.trim()).expect("initialize JSON");
    assert_eq!(
        init["result"]["serverInfo"]["name"], "lsp-cpp",
        "initialize body: {init}"
    );
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
}

/// Issue a `tools/call` for `lsp_cpp_status` and parse the embedded
/// payload. Returns `(clangd_pid, restart_count)`.
fn read_status(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<std::process::ChildStdout>,
    req_id: u64,
) -> (Option<u64>, u64) {
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":{req_id},"method":"tools/call","params":{{"name":"lsp_cpp_status","arguments":{{}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read status reply");
    let reply: Value = serde_json::from_str(line.trim()).expect("status JSON");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing content.text in status reply: {reply}"));
    let payload: Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("status payload not JSON: {e}; raw: {text}"));
    let pid = payload["clangd_pid"].as_u64();
    let restarts = payload["restart_count"].as_u64().unwrap_or_default();
    (pid, restarts)
}

#[test]
fn build_full_index_preserves_child_and_seeds_symbol_table() {
    if !clangd_available() {
        eprintln!(
            "build_full_index_preserves_child_and_seeds_symbol_table: skipped \
             (clangd-19 not on PATH; set CLANGD_BIN to override)"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path().join("proj");
    build_fixture(&project_root).expect("fixture");

    let (mut child, mut stdin, mut stdout) = spawn_shim(&project_root);
    handshake(&mut stdin, &mut stdout);

    // Trigger a backend `spawn()` BEFORE the build_full_index call so
    // we can capture the original clangd pid. workspace_symbol on an
    // empty in-memory index returns [] quickly and is the cheapest way
    // to force the lazy backend.spawn() path.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"workspace_symbol","arguments":{{"query":"NoSuchSymbol_v03"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read workspace_symbol pre reply");
    // Don't assert content — we only needed the spawn side-effect.

    let (pid_before, restarts_before) = read_status(&mut stdin, &mut stdout, 3);
    let pid_before = pid_before.expect("clangd_pid populated after backend.spawn()");

    // Drive build_full_index with a tiny cap. The fixture has one TU
    // so max_tus=5 is more than enough.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"build_full_index","arguments":{{"max_tus":5}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    line.clear();
    stdout
        .read_line(&mut line)
        .expect("read build_full_index reply");
    let bfi: Value = serde_json::from_str(line.trim()).expect("build_full_index JSON");
    let bfi_text = bfi["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing content.text in {bfi}"));
    let report: Value = serde_json::from_str(bfi_text)
        .unwrap_or_else(|e| panic!("build_full_index report not JSON: {e}; raw: {bfi_text}"));
    assert!(
        report["tus_opened"].as_u64().unwrap_or_default() >= 1,
        "build_full_index opened no TUs; report = {report}"
    );

    // **The deletion guard.** v0.2 SIGKILLed clangd here; v0.3 must not.
    let (pid_after, restarts_after) = read_status(&mut stdin, &mut stdout, 5);
    let pid_after = pid_after.expect("clangd_pid populated after build_full_index");
    assert_eq!(
        pid_before, pid_after,
        "clangd subprocess was replaced across build_full_index call \
         (before pid={pid_before}, after pid={pid_after}). v0.2 SIGKILLed the \
         child at end of build_full_index; v0.3 must keep it alive."
    );
    assert_eq!(
        restarts_before, restarts_after,
        "supervisor restart_count changed across build_full_index call \
         (before={restarts_before}, after={restarts_after}). The wrapped clangd \
         must survive build_full_index without supervisor intervention."
    );

    // Subsequent workspace_symbol must hit the symbol the fixture TU
    // defines — proving build_full_index actually populated the
    // in-memory index of the SAME long-lived clangd that we just
    // verified is still alive (rather than relying on a fresh spawn
    // re-scanning the project). Retry briefly because clangd parses
    // didOpens asynchronously.
    let mut attempt = 0u32;
    let deadline = Instant::now() + Duration::from_millis(RETRY_TOTAL_MS);
    let hits = loop {
        attempt += 1;
        let req_id = 100 + attempt as u64;
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":{req_id},"method":"tools/call","params":{{"name":"workspace_symbol","arguments":{{"query":"UniqueFixtureWidget_v03"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
        line.clear();
        stdout
            .read_line(&mut line)
            .expect("read workspace_symbol post reply");
        let reply: Value = serde_json::from_str(line.trim()).expect("workspace_symbol JSON");
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("missing content.text in {reply}"));
        let symbols: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("symbols payload not JSON: {e}; raw: {text}"));
        let arr = symbols
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("symbols payload not array: {symbols}"));
        eprintln!("attempt {attempt}: UniqueFixtureWidget_v03 hits = {}", arr.len());
        if !arr.is_empty() {
            break arr;
        }
        if Instant::now() >= deadline {
            panic!(
                "expected at least one UniqueFixtureWidget_v03 hit after \
                 build_full_index within {RETRY_TOTAL_MS}ms (attempts = {attempt}); \
                 got [] every time. The wrapped clangd likely died — without \
                 the lifetime fix v0.2 SIGKILLs the child at end of \
                 build_full_index and ensure_spawned spawns a fresh one with \
                 an empty index."
            );
        }
        std::thread::sleep(Duration::from_millis(RETRY_SLEEP_MS));
    };
    assert!(
        !hits.is_empty(),
        "UniqueFixtureWidget_v03 should be queryable after build_full_index"
    );

    drop(stdin);
    let _ = child.wait();
}
