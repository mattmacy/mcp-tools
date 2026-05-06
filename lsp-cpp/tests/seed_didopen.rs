//! Live integration test for the post-`initialize` seed-didOpen path.
//!
//! Spawns the `lsp-cpp serve-mcp` binary, drives the MCP stdio
//! handshake (`initialize` + `notifications/initialized`), then issues
//! `tools/call workspace_symbol` for an operator-supplied symbol that
//! lives in one of the seeded headers and asserts at least one hit.
//!
//! Without the seed-didOpen step (env var unset / empty) this query
//! typically returns an empty array on a cold cache — clangd's
//! in-memory index is empty post-initialize and the on-disk shards
//! have not yet been populated.
//!
//! Gated behind `LSP_CPP_LIVE_TEST=1`. Requires:
//!
//! - real `clangd-19` on PATH
//! - `LSP_PROJECT` pointing at a project root that has a populated
//!   `compile_commands.json`
//! - `LSP_CPP_SEED_HEADERS` set to a comma-separated list of absolute
//!   header paths whose translation-unit closures contain the query
//!   symbol
//! - `LSP_CPP_QUERY_SYMBOL` set to the symbol name to look up

use lsp_cpp::compat::live_test_env;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Per-attempt sleep between workspace_symbol retries while clangd
/// chews through the seeded didOpen parses asynchronously. Each
/// `textDocument/didOpen` triggers a TU parse on a worker thread; large
/// transitive header closures can take 30-60 s on a cold cache.
const RETRY_SLEEP_MS: u64 = 5000;
/// Maximum wall-clock wait for the symbol table to populate. Pass-fast
/// when the cache is warm; fail loudly when seed-didOpen never triggers
/// a parse.
const RETRY_TOTAL_MS: u64 = 120_000;

fn live_test_enabled() -> bool {
    live_test_env()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn shim_binary() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_lsp-cpp"))
}

#[test]
fn seed_didopen_makes_seeded_symbols_resolvable() {
    if !live_test_enabled() {
        eprintln!("seed_didopen_makes_seeded_symbols_resolvable: skipped (set LSP_CPP_LIVE_TEST=1 to run)");
        return;
    }
    let query_symbol = match std::env::var("LSP_CPP_QUERY_SYMBOL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("seed_didopen_makes_seeded_symbols_resolvable: skipped (set LSP_CPP_QUERY_SYMBOL to a symbol that exists in the seeded headers)");
            return;
        }
    };

    let bin = shim_binary();
    let mut child = Command::new(&bin)
        .arg("serve-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn lsp-cpp serve-mcp");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    // 1. initialize
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

    // 2. notifications/initialized — no reply expected.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    // 3. tools/call workspace_symbol — retried until clangd's
    //    in-memory index is populated by the seeded didOpen parses
    //    (which run asynchronously on a worker thread inside clangd
    //    after our `textDocument/didOpen` notifications land). The
    //    first tools/call triggers `backend.spawn()`, which is also
    //    where seed-didOpen fires; subsequent calls re-use the same
    //    long-lived clangd subprocess so they never re-pay the spawn
    //    cost. We poll until non-empty or deadline.
    let mut attempt = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_millis(RETRY_TOTAL_MS);
    let arr = loop {
        attempt += 1;
        let req_id = 100 + attempt as u64;
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":{req_id},"method":"tools/call","params":{{"name":"workspace_symbol","arguments":{{"query":"{query_symbol}"}}}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
        line.clear();
        stdout
            .read_line(&mut line)
            .expect("read workspace_symbol reply");
        let reply: Value = serde_json::from_str(line.trim()).expect("workspace_symbol JSON");
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("missing content.text in {reply}"));
        let symbols: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("symbols payload not JSON: {e}; raw: {text}"));
        let hits = symbols
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("symbols payload not array: {symbols}"));
        eprintln!(
            "attempt {attempt}: {query_symbol} hits = {}",
            hits.len()
        );
        if !hits.is_empty() {
            break hits;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "expected at least one {query_symbol} hit after seed-didOpen \
                 within {RETRY_TOTAL_MS}ms (attempts = {attempt}); got [] every time. \
                 Last reply: {reply}"
            );
        }
        std::thread::sleep(Duration::from_millis(RETRY_SLEEP_MS));
    };
    eprintln!(
        "{query_symbol} hits: {}; first: {}",
        arr.len(),
        arr[0]
    );

    // Clean shutdown — best effort; if clangd is wedged we just kill.
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"shutdown"}}"#).ok();
    stdin.flush().ok();
    drop(stdin);
    let _ = child.wait();
}
