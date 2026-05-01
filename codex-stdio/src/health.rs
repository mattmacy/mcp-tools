//! `codex_health` tool — reports whether the Codex shim can serve
//! requests right now. The cheap-Claude routing skill (sister branch
//! `cheap-claude-routing-skill`) calls this before classifying a task
//! to decide whether the Codex tier is in the candidate set.
//!
//! "Available" means: a transport can be built. For the production
//! path that means the `codex` CLI binary is on disk + executable;
//! for the replay path it means `CODEX_STDIO_REPLAY_FIXTURE` is set.
//! Health does NOT actually invoke `codex exec` (that would either
//! burn tokens or block on `codex login` if creds are missing). The
//! routing skill is responsible for falling back when run-task
//! returns an auth error.
//!
//! Latency reported is the time spent locating the binary + reading
//! env vars, which is a useful proxy for filesystem-stall detection
//! when a replay fixture path is on a networked mount or `which
//! codex` is slow.

use std::time::Instant;

use serde_json::{json, Value};

use crate::codex;
use crate::DEFAULT_MODEL;

/// Run the `codex_health` tool. Always returns `Ok` — unavailability
/// is a payload-shape concern (`{available: false, reason}`), not an
/// MCP-level error, so the routing skill can switch tiers without
/// crashing.
pub fn run() -> Result<Value, String> {
    let start = Instant::now();
    let result = probe_transport();
    let latency_ms = start.elapsed().as_millis() as u64;

    let model = std::env::var("CODEX_STDIO_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    match result {
        Ok(binary_path) => Ok(json!({
            "available": true,
            "model": model,
            "latency_ms": latency_ms,
            "transport": transport_kind(),
            "binary_path": binary_path,
        })),
        Err(reason) => Ok(json!({
            "available": false,
            "model": model,
            "latency_ms": latency_ms,
            "reason": reason,
            "transport": transport_kind(),
        })),
    }
}

/// Probe whether a transport can be constructed. Returns the
/// resolved binary path (or "(replay fixture)") on success.
fn probe_transport() -> Result<String, String> {
    if std::env::var("CODEX_STDIO_REPLAY_FIXTURE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Ok("(replay fixture)".into());
    }
    let bin = codex::locate_codex_binary()?;
    Ok(bin.display().to_string())
}

/// Which transport `from_env` would pick — for operator triage.
/// Cheap re-read of the same env vars [`codex::from_env`] consults.
fn transport_kind() -> &'static str {
    if std::env::var("CODEX_STDIO_REPLAY_FIXTURE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "replay"
    } else {
        "http"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutation-equivalent test: hard-coding `available: true` makes
    /// this assertion fail because `reason` would be absent. Counter-
    /// only would not catch that; we assert on the behavioural
    /// payload shape.
    #[test]
    fn missing_codex_binary_returns_unavailable_with_reason() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
        let prev_bin = std::env::var("CODEX_STDIO_BIN").ok();
        std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE");
        // Hostile override forces locate_codex_binary to error
        // even on a system that has /usr/bin/codex installed (the
        // bring-up container does), so this test is reproducible
        // regardless of the host's codex install state.
        std::env::set_var("CODEX_STDIO_BIN", "/no/such/binary/ever");

        let v = run().unwrap();
        assert_eq!(v["available"], false);
        let reason = v["reason"].as_str().expect("reason field present");
        // Mutation-equivalent: hard-coding `available: true` makes
        // this fail because `reason` would be absent. We assert on
        // the CODEX_STDIO_BIN substring so a false-positive `which
        // codex` lookup that resolved to some unrelated binary
        // wouldn't accidentally pass.
        assert!(reason.contains("CODEX_STDIO_BIN"), "got reason: {reason:?}");
        assert!(v["latency_ms"].as_u64().is_some());
        assert_eq!(v["transport"], "http");

        match prev_fixture {
            Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
            None => {}
        }
        match prev_bin {
            Some(v) => std::env::set_var("CODEX_STDIO_BIN", v),
            None => std::env::remove_var("CODEX_STDIO_BIN"),
        }
    }
}
