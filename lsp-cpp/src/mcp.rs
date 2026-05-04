//! MCP (Model Context Protocol) stdio server.
//!
//! Implements the subset of the MCP wire protocol that Claude Code's
//! `.mcp.json` runtime speaks: JSON-RPC 2.0 over stdio with
//! newline-delimited messages (NOT LSP-style `Content-Length` framing —
//! MCP and LSP are distinct protocols sharing the same JSON-RPC core).
//!
//! Methods served:
//!
//! - `initialize` — handshake. Returns `serverInfo` + `capabilities`
//!   advertising `tools` support.
//! - `notifications/initialized` — client-side notification, no-op.
//! - `tools/list` — returns the six tool surface this shim exposes
//!   (`workspace_symbol`, `definition`, `references`, `hover`,
//!   `lsp_cpp_status`, `build_full_index`). Each tool entry
//!   carries an `inputSchema` describing its arguments so the model
//!   can call them correctly.
//! - `tools/call` — dispatches to the [`LspBackend`] held in the server
//!   loop. Routes by `params.name`; arguments live under
//!   `params.arguments`.
//!
//! ## Why a long-lived backend handle
//!
//! Spawning a fresh clangd per `tools/call` would re-pay the multi-minute
//! cold-start cost (compile_commands scan + PCH parse). The server loop
//! holds one [`Clangd`] instance, calls `spawn()` on first use, and
//! re-uses the same subprocess for every subsequent request — matching
//! the design rationale called out in `lib.rs` ("One long-lived clangd
//! process per shim").
//!
//! ## Structured errors
//!
//! Every backend failure is converted to a JSON-RPC `error` response with
//! a stable `error_kind` field in `data`, mirroring the CLI's
//! `error_kind` mapping. This is the explicit cure for the "silent null"
//! pattern the previous upstream clangd-MCP fork shipped (see
//! `error.rs` module docstring) — callers that need to distinguish
//! "symbol not found" from "indexer crashed" can now branch on
//! `error.data.error_kind`.

use crate::backend::LspBackend;
use crate::clangd::{Clangd, BUILD_FULL_INDEX_DEFAULT_MAX_TUS};
use crate::error::ShimError;
use crate::queue::{BoundedQueue, DEFAULT_QUEUE_DEPTH};
use crate::supervisor::{RestartReason, RetryDecision, SupervisorPolicy};
use lsp_shim_core::mcp_proto::{code, method};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

/// Default suggested retry interval (seconds) embedded in
/// `queue_depth_exceeded` errors. 1 s is conservative — clangd query
/// turnaround is typically sub-second on a hot index, so a busy queue
/// usually drains in << 1 s.
const QUEUE_RETRY_AFTER_S: u64 = 1;

/// JSON-RPC error code for the supervisor's max-retries fail state.
/// Picks a value outside the standard JSON-RPC reserved range
/// (-32768..-32000) to avoid colliding with future spec additions.
/// Surfaced through the `error_kind = supervisor_max_retries` data tag
/// so callers can branch on the structured taxonomy.
const SUPERVISOR_MAX_RETRIES_CODE: i64 = -33000;

/// Run the MCP server loop on the given stdio handles until EOF.
///
/// Splitting this from `serve_stdio` lets tests drive the loop with
/// in-memory pipes. This entry point uses the system clock for the
/// supervisor; tests that need deterministic clock control build their
/// own [`SupervisorPolicy`] and call
/// [`serve_with_queue_and_supervisor`] directly.
pub fn serve<R, W>(reader: R, writer: W, backend: Clangd) -> std::io::Result<()>
where
    R: Read,
    W: Write,
{
    serve_with_queue_and_supervisor(
        reader,
        writer,
        backend,
        BoundedQueue::new(DEFAULT_QUEUE_DEPTH),
        SupervisorPolicy::with_system_clock(),
    )
}

/// Variant of [`serve`] that takes an explicit [`BoundedQueue`]. Used
/// by tests to drive depth-exceeded paths with a depth that fits inside
/// a single test scope. Threads a default-system-clock supervisor;
/// tests that also need deterministic supervisor control should call
/// [`serve_with_queue_and_supervisor`] instead.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn serve_with_queue<R, W>(
    reader: R,
    writer: W,
    backend: Clangd,
    queue: BoundedQueue,
) -> std::io::Result<()>
where
    R: Read,
    W: Write,
{
    serve_with_queue_and_supervisor(
        reader,
        writer,
        backend,
        queue,
        SupervisorPolicy::with_system_clock(),
    )
}

/// Run the MCP server loop with caller-supplied admission queue +
/// supervisor. Layering at every `tools/call` boundary, in order:
///
/// 1. **status-tool early return** — bypasses queue + supervisor so
///    operators can probe wrapper health even when the wrapper is
///    saturated or in `Failed` state.
/// 2. **queue admission** — `try_acquire` returns the
///    `queue_depth_exceeded` error before any backend work; load-shed
///    must precede dispatch so a saturated wrapper does not magnify
///    pressure on the supervised clangd.
/// 3. **supervisor `should_retry`** — `Backoff` returns
///    `supervisor_backoff`; `Failed` returns `supervisor_max_retries`.
///    Both arms surface structured retry hints so the model never
///    sees a phantom broken pipe without the wrapper telling it the
///    supervisor has given up.
/// 4. **liveness probe + dispatch + post-bookkeeping** — `is_alive`
///    reaps any zombie before dispatch, then `record_spawn` /
///    `record_exit` capture the post-dispatch state for the next
///    call's backoff.
pub(crate) fn serve_with_queue_and_supervisor<R, W>(
    reader: R,
    mut writer: W,
    mut backend: Clangd,
    queue: BoundedQueue,
    mut supervisor: SupervisorPolicy,
) -> std::io::Result<()>
where
    R: Read,
    W: Write,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // EOF — client closed stdin.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = handle_message(trimmed, &mut backend, &queue, &mut supervisor);
        if let Some(response) = response {
            let body = serde_json::to_string(&response)
                .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"failed to serialize response: {e}"}}}}"#));
            writeln!(writer, "{body}")?;
            writer.flush()?;
        }
    }
}

/// Convenience wrapper around [`serve`] that uses `std::io::stdin` /
/// `std::io::stdout`. Returns the loop's exit status as an `ExitCode`
/// for the `main.rs` `serve-mcp` subcommand.
pub fn serve_stdio(backend: Clangd) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock(), backend)
}

/// Parse one inbound line and return the JSON-RPC response (or `None`
/// for notifications, which the spec says MUST NOT be replied to).
///
/// `queue` gates `tools/call` admission. `initialize` / `tools/list` /
/// `shutdown` bypass the queue because they don't touch clangd's stdin
/// (or in shutdown's case, the call IS the drain) and shouldn't be
/// rejected by load-shedding. `supervisor` is consulted inside
/// [`handle_tools_call`] so that the `lsp_cpp_status` tool can
/// bypass the supervisor's `should_retry` gate (operators must be able
/// to query why the supervisor gave up even from `Failed` state).
fn handle_message(
    line: &str,
    backend: &mut Clangd,
    queue: &BoundedQueue,
    supervisor: &mut SupervisorPolicy,
) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                code::PARSE_ERROR,
                format!("malformed JSON: {e}"),
                None,
            ));
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = request.get("id").is_none();
    let method = match request.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => {
            if is_notification {
                return None;
            }
            return Some(error_response(
                id,
                code::INVALID_PARAMS,
                "missing `method` field".into(),
                None,
            ));
        }
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method.as_str() {
        method::INITIALIZE => Some(success_response(id, initialize_result())),
        method::INITIALIZED => None, // Notification per MCP spec, no reply.
        method::TOOLS_LIST => Some(success_response(id, tools_list_result())),
        method::TOOLS_CALL => Some(handle_tools_call(id, params, backend, queue, supervisor)),
        method::SHUTDOWN => {
            let _ = backend.shutdown();
            Some(success_response(id, Value::Null))
        }
        other => {
            if is_notification {
                None
            } else {
                Some(error_response(
                    id,
                    code::METHOD_NOT_FOUND,
                    format!("unknown method: {other}"),
                    None,
                ))
            }
        }
    }
}

/// `initialize` response body.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "lsp-cpp",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `tools/list` response body. Schema kept narrow — every field that the
/// tool actually consumes is required, no optional positional aliases,
/// so the model cannot dispatch a malformed call.
fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "workspace_symbol",
                "description": "Search the indexed C++ workspace for symbols matching `query`. Returns up to N matches with file:line:column locations.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Substring or fuzzy match against symbol name." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "definition",
                "description": "Resolve the symbol at `file:line:column` to its definition site(s).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to a .cpp/.h under the indexed project." },
                        "line": { "type": "integer", "description": "1-based line number." },
                        "column": { "type": "integer", "description": "1-based column number." }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "references",
                "description": "Find every reference to the symbol at `file:line:column`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "column": { "type": "integer" }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "hover",
                "description": "Return clangd's hover documentation for the symbol at `file:line:column`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "column": { "type": "integer" }
                    },
                    "required": ["file", "line", "column"]
                }
            },
            {
                "name": "lsp_cpp_status",
                "description": "Return the supervisor's view of the wrapped clangd subprocess: {clangd_pid, alive, uptime_s, restart_count, last_restart_reason, last_exit_status, supervisor_state, queue_in_flight, queue_capacity, try_wait_total_ns, alive_probe_error?}. The `try_wait_total_ns` field is the cumulative wall cost (nanoseconds) of the supervisor's `Child::try_wait` liveness probe across the wrapper-process lifetime — Standing-Rule-14 perf-window accumulator for the per-request poll cadence; intended for offline regression checks. The `alive_probe_error` field is OPTIONAL: present (string `\"io error: <msg>\"`) only when the OS-level alive probe (`Child::try_wait`) returned an `io::Error`; omitted on the success path. Use this RPC instead of `kill -SIGHUP <wrapper-pid>` to probe wrapper health — sending SIGHUP terminates the wrapper itself (no auto-respawn under stdio MCP) and forces a CC-session restart, as happened on 2026-04-27 ~17:30 Z.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "build_full_index",
                "description": "Pre-warm clangd's persistent index by feeding the first `max_tus` translation units from the project's compile_commands.json (defaults to 5000 — large monorepos can have 100k+ entries and uncapped runs take many hours). Returns {tus_opened, skipped_io_errors, wall_seconds, shards_on_disk_after, shards_size_bytes}. Blocking call; tail /tmp/lsp-cpp.log for progress while in flight.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "max_tus": {
                            "type": "integer",
                            "description": "Maximum number of TUs to open (default 5000). Cap exists because the full DB is ~150k TUs and a no-cap run takes many hours. Lower values (e.g. 50) are useful for smoke-testing the wiring."
                        }
                    }
                }
            }
        ]
    })
}

/// `tools/call` dispatch. `params.name` selects the tool; arguments
/// live under `params.arguments`.
///
/// Layered admission control (each layer can short-circuit before
/// touching `backend`):
///
/// 1. **status-tool early return** — `lsp_cpp_status` bypasses
///    queue + supervisor so operators can probe wrapper health even
///    when the wrapper is saturated or in `Failed` state. Bypass is
///    load-bearing: the 2026-04-27 ~17:30 Z incident escalated
///    because the operator could not query supervisor state without
///    sending SIGHUP to the wrapper.
/// 2. **queue admission** — `try_acquire` returns
///    `queue_depth_exceeded` before any backend work. The `Slot` RAII
///    guard releases the in-flight count when the response is built,
///    so backend errors never leak slots.
/// 3. **supervisor `should_retry`** — `Backoff` returns
///    `supervisor_backoff`; `Failed` returns `supervisor_max_retries`
///    with the JSON-RPC code [`SUPERVISOR_MAX_RETRIES_CODE`].
/// 4. **liveness probe** — `is_alive` reaps any zombie clangd left
///    over from a previous request, so the supervisor's restart
///    counter sees the exit before the next dispatch tries to write
///    to a dead pipe.
/// 5. **dispatch + post-bookkeeping** — `record_spawn` on first
///    successful call, `record_exit` if the dispatch surfaces a
///    broken-pipe / `ClangdExited` error. `RequestTimeout` /
///    `ClangdBusy` deliberately do NOT count against the supervisor —
///    those mean clangd is alive-but-slow, not crashed.
fn handle_tools_call(
    id: Value,
    params: Value,
    backend: &mut Clangd,
    queue: &BoundedQueue,
    supervisor: &mut SupervisorPolicy,
) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => {
            return error_response(
                id,
                code::INVALID_PARAMS,
                "tools/call missing `name`".into(),
                None,
            );
        }
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    // Layer 1: status tool runs unconditionally — even in `Failed`
    // state, even when the queue is saturated, we want operators to
    // be able to query why we gave up. Bypasses the `try_acquire` +
    // `should_retry` gates.
    if name == "lsp_cpp_status" {
        // Probe child liveness eagerly so the IO-error path is
        // observable in the payload (`alive_probe_error`) rather
        // than silently swallowed as `alive=false`. See
        // `build_status_payload` docstring.
        //
        // Standing-Rule-14 perf window: the `try_wait`-backed
        // probe runs on every status RPC (operator polling cadence
        // can be aggressive). Capture wall-clock cost into the
        // supervisor's accumulator so the cumulative overhead is
        // observable across long-running sessions.
        let probe_start = std::time::Instant::now();
        let alive_result = backend.is_alive();
        supervisor.record_try_wait_ns(probe_start.elapsed().as_nanos() as u64);
        let payload = build_status_payload(backend, queue, supervisor, alive_result);
        return success_response(
            id,
            json!({
                "content": [
                    { "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap_or_default() }
                ],
                "isError": false,
            }),
        );
    }

    // Layer 2: queue admission. Slot drops on scope exit (after the
    // response is built) so backend errors don't leak slots. This is
    // the load-bearing call site for `BoundedQueue`; deleting the
    // gate makes the queue module dead code and the
    // queue_depth_exceeded test regresses.
    let _slot = match queue.try_acquire() {
        Some(slot) => slot,
        None => {
            return error_response(
                id,
                code::INTERNAL_ERROR,
                format!(
                    "wrapper queue full ({}/{}); retry after {}s",
                    queue.in_flight(),
                    queue.capacity(),
                    QUEUE_RETRY_AFTER_S
                ),
                Some(json!({
                    "error_kind": "queue_depth_exceeded",
                    "in_flight": queue.in_flight(),
                    "capacity": queue.capacity(),
                    "retry_after_s": QUEUE_RETRY_AFTER_S,
                })),
            );
        }
    };

    // Layer 3: consult supervisor. We do not sleep inside the request
    // handler because blocking the entire MCP loop on a 16 s sleep
    // starves any in-flight `lsp_cpp_status` poll the operator
    // might issue. Instead we surface the wait as a structured
    // `try_again_after` hint and let the caller re-issue.
    match supervisor.should_retry() {
        RetryDecision::Proceed => {}
        RetryDecision::Wait { wait } => {
            return error_response(
                id,
                code::INTERNAL_ERROR,
                format!(
                    "clangd is restarting (backoff {} s); retry after the supervisor finishes its respawn cycle",
                    wait.as_secs()
                ),
                Some(json!({
                    "error_kind": "supervisor_backoff",
                    "retry_after_s": wait.as_secs(),
                    "supervisor_state": supervisor_state_tag(supervisor),
                })),
            );
        }
        RetryDecision::Fail { retry_after } => {
            return error_response(
                id,
                SUPERVISOR_MAX_RETRIES_CODE,
                format!(
                    "clangd has crashed {} times in the last {}s — supervisor giving up; window expires in {}s",
                    supervisor.total_restarts(),
                    crate::supervisor::RESTART_WINDOW.as_secs(),
                    retry_after.as_secs()
                ),
                Some(json!({
                    "error_kind": "supervisor_max_retries",
                    "retry_after_s": retry_after.as_secs(),
                    "total_restarts": supervisor.total_restarts(),
                    "last_restart_reason": supervisor
                        .last_reason()
                        .map(|r| r.as_tag())
                        .unwrap_or("none"),
                })),
            );
        }
    }

    // Layer 4: liveness probe before dispatch. If the previous request
    // left clangd in zombie state, observe it now so the supervisor
    // counts the exit and the next dispatch through `ensure_spawned()`
    // will start a fresh child. Keeps the broken-pipe path from
    // masquerading as a one-off `ShimError::Io`.
    //
    // Standing-Rule-14 perf window: same accumulator as the status
    // path above. Every dispatched MCP request crosses this gate
    // exactly once, so the counter scales with request count and
    // gives a clean `cumulative_ns / requests = mean probe cost`
    // post-merge regression check.
    let probe_start = std::time::Instant::now();
    let pre_dispatch_alive = backend.is_alive();
    supervisor.record_try_wait_ns(probe_start.elapsed().as_nanos() as u64);
    if let Ok(false) = pre_dispatch_alive {
        let reason = exit_status_to_reason(backend.last_exit_status());
        supervisor.record_exit(reason);
    }

    // Layer 5: dispatch.
    let backend_result: std::result::Result<Value, ShimError> = match name.as_str() {
        "workspace_symbol" => call_workspace_symbol(backend, &args),
        "definition" => call_position(backend, &args, PositionKind::Definition),
        "references" => call_position(backend, &args, PositionKind::References),
        "hover" => call_position(backend, &args, PositionKind::Hover),
        "build_full_index" => call_build_full_index(backend, &args),
        other => {
            return error_response(
                id,
                code::METHOD_NOT_FOUND,
                format!("unknown tool: {other}"),
                Some(json!({ "error_kind": "unknown_tool" })),
            );
        }
    };

    // Post-dispatch supervisor bookkeeping. The dispatch above may
    // have called `backend.spawn()` for the first time (lazy spawn on
    // first request) — tell the supervisor so its uptime + healthy-
    // window logic starts ticking. On error we classify the failure
    // and record an exit so the next dispatch picks up the right
    // backoff.
    record_dispatch_outcome(supervisor, &backend_result);

    match backend_result {
        Ok(payload) => success_response(
            id,
            json!({
                "content": [
                    { "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap_or_default() }
                ],
                "isError": false,
            }),
        ),
        Err(e) => error_response(
            id,
            code::INTERNAL_ERROR,
            format!("{e}"),
            Some(json!({ "error_kind": error_kind(&e) })),
        ),
    }
}

/// Translate a `last_exit_status` string from `Clangd::is_alive` into
/// the supervisor's `RestartReason`. `code=0` is treated as a
/// `ChildExited { code: 0 }` because clangd should never exit
/// voluntarily during an MCP session.
fn exit_status_to_reason(status: Option<&str>) -> RestartReason {
    let s = match status {
        Some(s) => s,
        None => {
            return RestartReason::WaitFailed {
                message: "child gone, status unknown".into(),
            }
        }
    };
    if let Some(code_str) = s.strip_prefix("code=") {
        if code_str == "?" {
            return RestartReason::WaitFailed {
                message: "platform did not provide exit status".into(),
            };
        }
        if let Ok(code) = code_str.parse::<i32>() {
            return RestartReason::ChildExited { code };
        }
    }
    if let Some(sig_str) = s.strip_prefix("signal=") {
        if let Ok(sig) = sig_str.parse::<i32>() {
            return RestartReason::ChildSignaled { signal: sig };
        }
    }
    RestartReason::WaitFailed {
        message: format!("unparsed exit status {s:?}"),
    }
}

/// Determine whether an in-flight request error indicates a dead
/// clangd. Broken-pipe / EOF I/O errors are the canonical signature
/// of the 2026-04-27 ~17:30 Z incident: blocking `request()` writes
/// succeeded but the next `recv()` returned EOF because the child
/// closed stdout. `RequestTimeout` and `ClangdBusy` are NOT
/// child-exit signals — clangd is alive but stuck on a single TU,
/// the supervisor should leave it alone (the queue's busy-vs-broken
/// taxonomy already disambiguates that case).
fn classify_request_error(e: &ShimError) -> Option<RestartReason> {
    match e {
        ShimError::Io(io_err) => {
            use std::io::ErrorKind;
            match io_err.kind() {
                ErrorKind::BrokenPipe
                | ErrorKind::UnexpectedEof
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionReset => Some(RestartReason::BrokenPipe),
                _ => None,
            }
        }
        ShimError::ClangdExited { status, .. } => Some(exit_status_to_reason(Some(status))),
        _ => None,
    }
}

/// Apply post-dispatch supervisor bookkeeping for one `tools/call`.
///
/// On success the dispatch proves clangd is alive — record a spawn
/// regardless of the prior state. The transition matrix is:
///
/// | prior state    | next state |
/// |----------------|------------|
/// | `Stopped`      | `Running`  |
/// | `Backoff{d}`   | `Running`  |
/// | `Running`      | `Running` (idempotent — refreshes `last_spawn_ns`) |
/// | `Failed`       | `Running` (only reachable if `should_retry` already \
/// |                |   demoted Failed→Stopped via window expiry on this   \
/// |                |   request, otherwise we never got here)              |
///
/// The `Backoff → Running` arc was previously unreachable in the
/// production path: `record_spawn` was gated on `state == Stopped`,
/// so after Layer 4 (`record_exit`) demoted the state to `Backoff`,
/// the lazily re-spawned clangd succeeded but the supervisor never
/// noticed — every subsequent request hit `should_retry → Wait` and
/// emitted `supervisor_backoff` indefinitely. See the regression test
/// `dispatch_success_in_backoff_transitions_to_running` for the
/// counterfactual that fails on the broken gate.
///
/// On error we classify the failure and record an exit so the next
/// dispatch picks up the right backoff. `RequestTimeout` / `ClangdBusy`
/// are NOT child-exit signals — clangd is alive-but-stuck — so
/// `classify_request_error` returns `None` for them and we leave the
/// supervisor in `Running`.
fn record_dispatch_outcome(
    supervisor: &mut SupervisorPolicy,
    result: &std::result::Result<Value, ShimError>,
) {
    match result {
        Ok(_) => supervisor.record_spawn(),
        Err(e) => {
            if let Some(reason) = classify_request_error(e) {
                supervisor.record_exit(reason);
            }
        }
    }
}

/// Build the JSON payload for the `lsp_cpp_status` tool. Carries
/// both supervisor + queue state so a single status probe captures
/// the full wrapper-health picture (post-integration: pre-merge the
/// supervisor and queue branches each held half).
///
/// `alive_result` is the (already-evaluated) outcome of
/// [`Clangd::is_alive`]. Threading it as a parameter (rather than
/// calling `backend.is_alive()` here) keeps this function pure —
/// tests can inject a synthetic `Err` to drive the `alive_probe_error`
/// surface without needing a real `Child` whose `try_wait()` fails
/// (a state notoriously hard to construct on Unix).
///
/// IO-error policy: prior code shape was
/// `backend.is_alive().unwrap_or(false)`, which silently conflated
/// "child IO probe failed" (e.g. EBADF on a moved-out child handle,
/// EINTR not retried in libstd) with "child has exited cleanly." A
/// dead clangd would surface a structured `last_exit_status` +
/// `RestartReason`, but a broken probe surfaced as `alive=false`
/// with no diagnostic — operators saw "supervisor restarting" with
/// no cause. Now: on `Err`, `alive=false` AND `alive_probe_error`
/// carries the IO error's `Display` rendering. On `Ok`, the field
/// is absent (so steady-state payloads don't carry a null sentinel).
fn build_status_payload(
    backend: &mut Clangd,
    queue: &BoundedQueue,
    supervisor: &SupervisorPolicy,
    alive_result: std::io::Result<bool>,
) -> Value {
    let (alive, alive_probe_error) = match alive_result {
        Ok(b) => (b, None),
        Err(e) => (false, Some(format!("io error: {e}"))),
    };
    let mut payload = json!({
        "clangd_pid": backend.clangd_pid(),
        "alive": alive,
        "uptime_s": supervisor.current_uptime().as_secs(),
        "restart_count": supervisor.total_restarts(),
        "last_restart_reason": supervisor
            .last_reason()
            .map(|r| r.as_tag())
            .unwrap_or("none"),
        "last_exit_status": backend.last_exit_status(),
        "supervisor_state": supervisor_state_tag(supervisor),
        "queue_in_flight": queue.in_flight(),
        "queue_capacity": queue.capacity(),
        // Standing-Rule-14 perf-window accumulator: cumulative wall
        // cost of the supervisor's `Child::try_wait` liveness probe
        // across the lifetime of this wrapper process. Surfaced for
        // offline regression checks (compare `try_wait_total_ns /
        // restart_count` mean against a baseline); not consulted by
        // any branch in this wrapper.
        "try_wait_total_ns": supervisor.try_wait_total_ns(),
    });
    if let Some(err) = alive_probe_error {
        payload
            .as_object_mut()
            .expect("json! produces an object")
            .insert("alive_probe_error".to_string(), Value::String(err));
    }
    payload
}

/// Stable string tag for the supervisor's logical state (used in both
/// the status tool payload and the error-response `data` blocks).
fn supervisor_state_tag(supervisor: &SupervisorPolicy) -> &'static str {
    use crate::supervisor::SupervisorState;
    match supervisor.current_state() {
        SupervisorState::Stopped => "stopped",
        SupervisorState::Running => "running",
        SupervisorState::Backoff { .. } => "backoff",
        SupervisorState::Failed => "failed",
    }
}

enum PositionKind {
    Definition,
    References,
    Hover,
}

fn call_workspace_symbol(
    backend: &mut Clangd,
    args: &Value,
) -> std::result::Result<Value, ShimError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ShimError::Protocol("workspace_symbol requires `query` (string)".into()))?;
    if query.is_empty() {
        return Err(ShimError::Protocol(
            "workspace_symbol `query` must not be empty".into(),
        ));
    }
    backend.spawn()?;
    let symbols = backend.workspace_symbol(query)?;
    Ok(serde_json::to_value(symbols)?)
}

/// Dispatch the `build_full_index` MCP tool. Parses the optional
/// `max_tus` argument (defaults to
/// [`BUILD_FULL_INDEX_DEFAULT_MAX_TUS`]) and forwards to
/// [`Clangd::build_full_index`]. The returned report is wrapped in a
/// `serde_json::Value` for the MCP `tools/call` envelope.
pub(crate) fn call_build_full_index(
    backend: &mut Clangd,
    args: &Value,
) -> std::result::Result<Value, ShimError> {
    let max_tus = args
        .get("max_tus")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(BUILD_FULL_INDEX_DEFAULT_MAX_TUS);
    if max_tus == 0 {
        return Err(ShimError::Protocol(
            "build_full_index `max_tus` must be > 0".into(),
        ));
    }
    let report = backend.build_full_index(max_tus)?;
    Ok(serde_json::to_value(report)?)
}

fn call_position(
    backend: &mut Clangd,
    args: &Value,
    kind: PositionKind,
) -> std::result::Result<Value, ShimError> {
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| ShimError::Protocol("missing `file` (string)".into()))?;
    let line =
        args.get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| ShimError::Protocol("missing `line` (integer)".into()))? as u32;
    let column = args
        .get("column")
        .and_then(Value::as_u64)
        .ok_or_else(|| ShimError::Protocol("missing `column` (integer)".into()))?
        as u32;
    backend.spawn()?;
    match kind {
        PositionKind::Definition => Ok(serde_json::to_value(
            backend.definition(file, line, column)?,
        )?),
        PositionKind::References => Ok(serde_json::to_value(
            backend.references(file, line, column)?,
        )?),
        PositionKind::Hover => Ok(serde_json::to_value(backend.hover(file, line, column)?)?),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: String, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(data) = data {
        error.as_object_mut().unwrap().insert("data".into(), data);
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
}

/// Stable string tag for every `ShimError` variant. Mirrors the CLI's
/// `error_kind()` so callers consuming both transports see the same
/// taxonomy.
fn error_kind(e: &ShimError) -> &'static str {
    match e {
        ShimError::ClangdMissing { .. } => "clangd_missing",
        ShimError::NoCompileCommands { .. } => "no_compile_commands",
        ShimError::NoIndexFile { .. } => "no_index_file",
        ShimError::InitializeTimeout { .. } => "initialize_timeout",
        ShimError::WarmupTimeout { .. } => "warmup_timeout",
        ShimError::RequestTimeout { .. } => "request_timeout",
        ShimError::ClangdBusy { .. } => "clangd_busy",
        ShimError::QueueDepthExceeded { .. } => "queue_depth_exceeded",
        ShimError::ClangdExited { .. } => "clangd_exited",
        ShimError::Protocol(_) => "protocol",
        ShimError::Io(_) => "io",
        ShimError::Json(_) => "json",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tools/list` returns exactly the six tools the shim exposes,
    /// each with a non-empty `inputSchema`. Deleting any `tools` array
    /// entry in `tools_list_result` makes this test fail — Rule 11
    /// "delete the code, does the test fail?" satisfied. Five of the
    /// tools cover LSP queries; the sixth, `lsp_cpp_status`,
    /// surfaces supervisor + queue state so the operator can probe
    /// wrapper health without sending SIGHUP (the failure mode that
    /// triggered the 2026-04-27 ~17:30 Z incident).
    #[test]
    fn tools_list_enumerates_six_tools_with_schemas() {
        let result = tools_list_result();
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6, "expected 6 tools, got {}", tools.len());
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names,
            [
                "workspace_symbol",
                "definition",
                "references",
                "hover",
                "lsp_cpp_status",
                "build_full_index"
            ]
        );
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    /// `initialize` advertises tools capability + correct server name.
    /// Deleting either field fails the test.
    #[test]
    fn initialize_advertises_tools_capability() {
        let result = initialize_result();
        assert_eq!(result["serverInfo"]["name"], "lsp-cpp");
        assert!(
            result["capabilities"]["tools"].is_object(),
            "tools capability missing"
        );
        assert!(
            result["protocolVersion"].is_string(),
            "protocolVersion missing"
        );
    }

    /// Notification (no `id`) for `notifications/initialized` returns
    /// no response per MCP spec — the entire serve() loop produces
    /// EMPTY stdout for a notification-only input. Reviewer
    /// (codebase-analyzer a83da932d14666385) flagged the previous
    /// fixture-only assertion (`request.get("id").is_none()`) as
    /// theatrical: it never invoked handle_message and would survive
    /// deletion of the notification-handling branch at mcp.rs:122 +
    /// the `if is_notification { None }` catch-all at mcp.rs:131.
    /// Drive serve() end-to-end mirroring
    /// `malformed_json_returns_parse_error_not_silent_null` shape.
    #[test]
    fn initialized_notification_yields_empty_serve_output() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n".to_vec();
        let mut output = Vec::new();
        let backend = Clangd::new("/path/to/project");
        serve(input.as_slice(), &mut output, backend).expect("serve loop");
        assert!(
            output.is_empty(),
            "notification MUST produce empty stdout per JSON-RPC spec; got {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    /// Malformed JSON line surfaces a structured PARSE_ERROR rather
    /// than crashing the loop or returning silent null. This is the
    /// explicit cure for the legacy fork's silent-null behaviour.
    #[test]
    fn malformed_json_returns_parse_error_not_silent_null() {
        // Drive serve() with a one-shot reader so we don't need a
        // real clangd. The first line is junk; we capture the response
        // and confirm code = -32700.
        let input = b"this is not json\n".to_vec();
        let mut output = Vec::new();
        // Stub backend — we never call it because PARSE_ERROR fires
        // before dispatch. Construct via the same constructor the
        // CLI uses.
        let backend = Clangd::new("/path/to/project");
        serve(input.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("malformed JSON"),
            "expected `malformed JSON` in message, got {parsed}"
        );
        // No silent null — `result` field MUST be absent on error.
        assert!(parsed.get("result").is_none());
    }

    /// `tools/call` with unknown tool name returns METHOD_NOT_FOUND
    /// with `error_kind=unknown_tool` in `data`. Confirms the
    /// structured-error contract the CLI's `error_kind` taxonomy
    /// promises also holds for the MCP transport.
    #[test]
    fn tools_call_unknown_tool_returns_structured_error() {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"does_not_exist","arguments":{{}}}}}}{}"#,
            "\n"
        );
        let mut output = Vec::new();
        let backend = Clangd::new("/path/to/project");
        serve(request.as_bytes(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(parsed["error"]["data"]["error_kind"], "unknown_tool");
    }

    /// `tools/list` over the wire returns the same six-tool body the
    /// pure-function helper does. End-to-end check that the request
    /// parser routes correctly.
    #[test]
    fn tools_list_request_returns_six_tools() {
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n".to_vec();
        let mut output = Vec::new();
        let backend = Clangd::new("/path/to/project");
        serve(request.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 2);
        let tools = parsed["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 6);
    }

    /// `initialize` over the wire returns the handshake body and does
    /// NOT touch the backend (no clangd spawn during initialize).
    #[test]
    fn initialize_request_does_not_spawn_clangd() {
        let request =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n".to_vec();
        let mut output = Vec::new();
        // Use a project root that does not exist. If initialize tried
        // to spawn clangd against it, ShimError::NoCompileCommands
        // would surface; instead we expect a clean success response.
        let backend = Clangd::new("/nonexistent/project/root");
        serve(request.as_slice(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["serverInfo"]["name"], "lsp-cpp");
        assert!(parsed.get("error").is_none(), "initialize should not error");
    }

    /// Queue rejection path: pre-fill the queue with one held slot
    /// (capacity=1), then send a `tools/call` request through serve.
    /// Expected: `error.data.error_kind == "queue_depth_exceeded"`,
    /// NOT `request_timeout` and NOT a silent broken-pipe surface.
    /// Counterfactual cover: deleting the `try_acquire` branch in
    /// `handle_message` (i.e., always dispatching) causes this test
    /// to fail because the call would attempt to spawn clangd against
    /// `/nonexistent/project/root` and surface `no_compile_commands`
    /// instead of `queue_depth_exceeded`.
    ///
    /// Rule 11: deleting the queue gate makes this test fail; the
    /// queue gate is therefore exercised by this test.
    #[test]
    fn tools_call_with_full_queue_returns_queue_depth_exceeded() {
        // Capacity 1, one slot held by the test → queue is full.
        let queue = BoundedQueue::new(1);
        let _held = queue.try_acquire().expect("test slot");
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\
            \"params\":{\"name\":\"workspace_symbol\",\"arguments\":{\"query\":\"FFrame\"}}}\n"
            .to_vec();
        let mut output = Vec::new();
        // Project root deliberately absent so that, IF the queue gate
        // were to be removed, the call would surface
        // `no_compile_commands` — making the regression observable.
        let backend = Clangd::new("/nonexistent/project/root");
        serve_with_queue(request.as_slice(), &mut output, backend, queue.clone())
            .expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 42);
        assert_eq!(
            parsed["error"]["data"]["error_kind"], "queue_depth_exceeded",
            "expected queue_depth_exceeded, got {}",
            parsed
        );
        assert_eq!(parsed["error"]["data"]["capacity"], 1);
        assert_eq!(parsed["error"]["data"]["in_flight"], 1);
        assert!(
            parsed["error"]["data"]["retry_after_s"].is_number(),
            "retry_after_s missing"
        );
        // Result field MUST be absent on error — no silent null.
        assert!(parsed.get("result").is_none());
    }

    /// `initialize` and `tools/list` MUST bypass the admission queue
    /// because they don't touch the backend; rejecting them on a full
    /// queue would deadlock initialization. Deleting the bypass (i.e.,
    /// gating those methods on the queue) would fail this test.
    #[test]
    fn handshake_methods_bypass_full_queue() {
        let queue = BoundedQueue::new(1);
        let _held = queue.try_acquire().expect("fill queue");
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
                        {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
            .to_vec();
        let mut output = Vec::new();
        let backend = Clangd::new("/nonexistent/project/root");
        serve_with_queue(request.as_slice(), &mut output, backend, queue).expect("serve loop");
        let body = String::from_utf8(output).expect("utf-8");
        // Two responses, each on its own line.
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 responses, got {body:?}");
        let init: Value = serde_json::from_str(lines[0]).expect("init JSON");
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["serverInfo"]["name"], "lsp-cpp");
        let list: Value = serde_json::from_str(lines[1]).expect("list JSON");
        assert_eq!(list["id"], 2);
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 6);
    }

    /// `lsp_cpp_status` is callable on a stopped supervisor + an
    /// uninitialised backend without spawning clangd. The payload
    /// carries the supervisor-state tag, restart count, queue depth,
    /// and PID (`null` when no child is alive). Counterfactual:
    /// gating the status tool behind `should_retry` (so it returned
    /// the supervisor_backoff error in `Backoff` state) would make
    /// this assertion fail because the call would surface as an
    /// error response, not a content payload. Same for the queue
    /// gate: routing the status tool through `try_acquire` would
    /// fail this assertion when the operator probes a saturated
    /// wrapper.
    #[test]
    fn status_tool_callable_without_spawning_clangd() {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{{"name":"lsp_cpp_status","arguments":{{}}}}}}{}"#,
            "\n"
        );
        let mut output = Vec::new();
        // Use a project root that does not exist. If the status tool
        // accidentally tried to spawn clangd against it,
        // ShimError::NoCompileCommands would surface; instead we
        // expect a clean `result.content[0].text` payload.
        let backend = Clangd::new("/nonexistent/project/root");
        serve(request.as_bytes(), &mut output, backend).expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 42);
        assert!(
            parsed.get("error").is_none(),
            "status tool must not error on a stopped supervisor; got {parsed}"
        );
        let payload_text = parsed["result"]["content"][0]["text"]
            .as_str()
            .expect("content text");
        let payload: Value =
            serde_json::from_str(payload_text).expect("status payload parses as JSON");
        assert_eq!(payload["clangd_pid"], Value::Null);
        assert_eq!(payload["alive"], false);
        assert_eq!(payload["supervisor_state"], "stopped");
        assert_eq!(payload["restart_count"], 0);
        // Queue fields exist (post-integration the status payload
        // covers BOTH supervisor + queue state in a single probe).
        assert!(payload["queue_capacity"].is_u64());
    }

    /// `Failed`-state supervisor surfaces a structured
    /// `supervisor_max_retries` error code, not a phantom internal
    /// error or silent null. Drives the production code path the
    /// 2026-04-27 ~17:30 Z incident exposed: when clangd has died
    /// too many times in succession, the wrapper must tell the
    /// model "we've given up — here's why," not silently resume
    /// serving requests that will all fail.
    ///
    /// Counterfactual: collapsing the `Fail` arm of `should_retry()`
    /// into `Proceed` would let the dispatch fall through to
    /// `call_workspace_symbol` (which would try to spawn clangd
    /// against /nonexistent and surface `NoCompileCommands`) and
    /// break the assertion on `error_kind = supervisor_max_retries`.
    #[test]
    fn failed_supervisor_returns_max_retries_error() {
        use crate::supervisor::{RestartReason, SupervisorPolicy};
        // Build a supervisor that's already in Failed state by
        // forcibly recording 5 exits.
        let mut supervisor = SupervisorPolicy::with_system_clock();
        for _ in 0..crate::supervisor::MAX_RESTARTS_IN_WINDOW {
            supervisor.record_spawn();
            supervisor.record_exit(RestartReason::BrokenPipe);
        }
        assert!(matches!(
            supervisor.current_state(),
            crate::supervisor::SupervisorState::Failed
        ));

        let request = format!(
            r#"{{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{{"name":"workspace_symbol","arguments":{{"query":"FFrame"}}}}}}{}"#,
            "\n"
        );
        let mut output = Vec::new();
        let backend = Clangd::new("/nonexistent/project/root");
        let queue = BoundedQueue::new(DEFAULT_QUEUE_DEPTH);
        serve_with_queue_and_supervisor(
            request.as_bytes(),
            &mut output,
            backend,
            queue,
            supervisor,
        )
        .expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 99);
        assert_eq!(parsed["error"]["code"], SUPERVISOR_MAX_RETRIES_CODE);
        assert_eq!(
            parsed["error"]["data"]["error_kind"],
            "supervisor_max_retries"
        );
        assert_eq!(parsed["error"]["data"]["total_restarts"], 5);
        assert_eq!(
            parsed["error"]["data"]["last_restart_reason"],
            "broken_pipe"
        );
        // retry_after_s is a positive integer (we did not advance
        // the clock; the window has not yet expired).
        let retry_after = parsed["error"]["data"]["retry_after_s"]
            .as_u64()
            .expect("retry_after_s present");
        assert!(retry_after > 0);
    }

    /// Status tool bypasses BOTH gates: the wrapper queue is
    /// saturated AND the supervisor is in `Failed` state, yet the
    /// operator can still probe `lsp_cpp_status` and receive
    /// a content payload (not an error). Drives the integration
    /// invariant the 2026-04-27 ~17:30 Z incident anchored: an
    /// operator must always be able to query wrapper health, even
    /// when both load-shedding and the supervisor have given up.
    ///
    /// Counterfactual A: routing the status tool through
    /// `try_acquire` makes the saturated-queue path return
    /// `queue_depth_exceeded` and breaks `parsed["error"].is_null()`.
    /// Counterfactual B: routing the status tool through
    /// `should_retry` makes the Failed supervisor return
    /// `supervisor_max_retries` and breaks the same assertion.
    #[test]
    fn status_tool_bypasses_queue_and_supervisor_gates() {
        use crate::supervisor::{RestartReason, SupervisorPolicy};

        // Saturate queue: capacity 1, hold the only slot.
        let queue = BoundedQueue::new(1);
        let _held = queue.try_acquire().expect("test slot");
        // Force supervisor into Failed state.
        let mut supervisor = SupervisorPolicy::with_system_clock();
        for _ in 0..crate::supervisor::MAX_RESTARTS_IN_WINDOW {
            supervisor.record_spawn();
            supervisor.record_exit(RestartReason::BrokenPipe);
        }
        assert!(matches!(
            supervisor.current_state(),
            crate::supervisor::SupervisorState::Failed
        ));

        let request = format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"lsp_cpp_status","arguments":{{}}}}}}{}"#,
            "\n"
        );
        let mut output = Vec::new();
        let backend = Clangd::new("/nonexistent/project/root");
        serve_with_queue_and_supervisor(
            request.as_bytes(),
            &mut output,
            backend,
            queue.clone(),
            supervisor,
        )
        .expect("serve loop");
        let reply = String::from_utf8(output).expect("utf-8");
        let parsed: Value = serde_json::from_str(reply.trim()).expect("response is JSON");
        assert_eq!(parsed["id"], 7);
        assert!(
            parsed.get("error").is_none(),
            "status tool must bypass both gates; got {parsed}"
        );
        let payload_text = parsed["result"]["content"][0]["text"]
            .as_str()
            .expect("content text");
        let payload: Value =
            serde_json::from_str(payload_text).expect("status payload parses as JSON");
        // Payload reports the saturated state we engineered.
        assert_eq!(payload["supervisor_state"], "failed");
        assert_eq!(payload["queue_in_flight"], 1);
        assert_eq!(payload["queue_capacity"], 1);
        assert_eq!(payload["restart_count"], 5);
    }

    /// `exit_status_to_reason` round-trip pins the parser used to
    /// classify a dead-clangd observation into a supervisor
    /// `RestartReason` tag for the status RPC.
    ///
    /// Counterfactual: replacing the `strip_prefix("signal=")` arm
    /// with `RestartReason::ChildExited { code: 0 }` would break the
    /// `child_signaled` assertion. Replacing the `strip_prefix("code=")`
    /// arm with the `WaitFailed` fallback would break the
    /// `child_exited` assertion.
    #[test]
    fn exit_status_to_reason_round_trip() {
        use crate::supervisor::RestartReason;
        assert!(matches!(
            exit_status_to_reason(Some("code=0")),
            RestartReason::ChildExited { code: 0 }
        ));
        assert!(matches!(
            exit_status_to_reason(Some("code=42")),
            RestartReason::ChildExited { code: 42 }
        ));
        assert!(matches!(
            exit_status_to_reason(Some("signal=9")),
            RestartReason::ChildSignaled { signal: 9 }
        ));
        assert!(matches!(
            exit_status_to_reason(Some("code=?")),
            RestartReason::WaitFailed { .. }
        ));
        assert!(matches!(
            exit_status_to_reason(None),
            RestartReason::WaitFailed { .. }
        ));
        assert_eq!(RestartReason::BrokenPipe.as_tag(), "broken_pipe");
    }

    /// Drives the full zombie-reap → successful-dispatch → next-request
    /// happy path through `record_dispatch_outcome`, the post-dispatch
    /// bookkeeping helper that wraps the supervisor mutation at
    /// `handle_tools_call`'s Layer 5. The 2026-04-27 BOUNCE_BACK
    /// (torvalds verdict on `lsp-cpp-supervisor-on-queue-integration`
    /// tip `b6884dc1`) found this path unreachable in production: the
    /// helper used to gate `record_spawn` on `state == Stopped`, so
    /// after Layer 4 (`record_exit`) put the supervisor into
    /// `Backoff{d}`, every subsequent successful dispatch left the state
    /// at `Backoff` and the next `should_retry()` returned `Wait`,
    /// emitting `supervisor_backoff` indefinitely.
    ///
    /// Test sequence (mirrors the production wiring step-by-step):
    ///
    /// 1. Initial spawn: `record_spawn` → state `Running`.
    /// 2. Simulate Layer 4 zombie observation: `record_exit` →
    ///    state `Backoff{1s}`.
    /// 3. Layer 5 dispatch succeeds (we synthesize `Ok(Value::Null)`):
    ///    `record_dispatch_outcome` → state must be `Running` again.
    ///    THIS is the assertion the broken gate fails — on `b6884dc1`
    ///    the state would still be `Backoff{1s}` here.
    /// 4. Second successful dispatch: `record_dispatch_outcome` →
    ///    state stays `Running`. Confirms the `Running` arm is
    ///    idempotent (refreshes `last_spawn_ns` without churning state).
    /// 5. Confirms `should_retry()` returns `Proceed` from `Running`,
    ///    which is the user-facing invariant: the second tools/call
    ///    after a zombie reap must NOT receive `supervisor_backoff`.
    ///
    /// Counterfactual: re-introduce the `if matches!(state, Stopped)`
    /// gate around `record_spawn` in `record_dispatch_outcome`.
    /// Step 3's `assert!(matches!(state, Running))` then fails because
    /// the supervisor stays in `Backoff{1s}` — exactly the production
    /// bug torvalds caught.
    #[test]
    fn dispatch_success_in_backoff_transitions_to_running() {
        use crate::supervisor::{RestartReason, RetryDecision, SupervisorPolicy, SupervisorState};

        let mut supervisor = SupervisorPolicy::with_system_clock();

        // Step 1: initial spawn establishes Running.
        supervisor.record_spawn();
        assert_eq!(*supervisor.current_state(), SupervisorState::Running);

        // Step 2: simulate Layer 4 zombie observation.
        supervisor.record_exit(RestartReason::BrokenPipe);
        match supervisor.current_state() {
            SupervisorState::Backoff { duration } => {
                assert_eq!(*duration, std::time::Duration::from_secs(1));
            }
            other => panic!("expected Backoff after record_exit, got {other:?}"),
        }

        // Step 3: Layer 5 dispatch succeeds. On the broken gate, the
        // helper's `Stopped`-only check skipped record_spawn here and
        // the state remained Backoff. After the fix, record_spawn
        // fires unconditionally on success and the state becomes
        // Running.
        let ok_result: std::result::Result<Value, ShimError> = Ok(Value::Null);
        record_dispatch_outcome(&mut supervisor, &ok_result);
        assert_eq!(
            *supervisor.current_state(),
            SupervisorState::Running,
            "Backoff→Running transition unreachable; supervisor stuck in {:?}",
            supervisor.current_state()
        );

        // Step 4: second successful dispatch is idempotent in Running.
        record_dispatch_outcome(&mut supervisor, &ok_result);
        assert_eq!(
            *supervisor.current_state(),
            SupervisorState::Running,
            "second dispatch must stay in Running; got {:?}",
            supervisor.current_state()
        );

        // Step 5: user-facing invariant — `should_retry()` returns
        // `Proceed` (not `Wait{1s}`) so the next tools/call goes
        // through cleanly, not as `supervisor_backoff`.
        assert_eq!(supervisor.should_retry(), RetryDecision::Proceed);
    }

    /// Pins the failure-side bookkeeping in `record_dispatch_outcome`:
    /// a broken-pipe `ShimError::Io` after a successful dispatch must
    /// produce a `record_exit(BrokenPipe)` so the supervisor demotes
    /// `Running → Backoff` and the next request takes the right
    /// branch. Distinct from
    /// `dispatch_success_in_backoff_transitions_to_running` because
    /// it asserts the OTHER arm of the helper (Err path), so a future
    /// edit that accidentally collapses both arms regresses one of
    /// the two tests.
    #[test]
    fn dispatch_broken_pipe_records_exit() {
        use crate::supervisor::{RestartReason, SupervisorPolicy, SupervisorState};

        let mut supervisor = SupervisorPolicy::with_system_clock();
        supervisor.record_spawn();
        assert_eq!(*supervisor.current_state(), SupervisorState::Running);

        let err_result: std::result::Result<Value, ShimError> = Err(ShimError::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "child closed stdout"),
        ));
        record_dispatch_outcome(&mut supervisor, &err_result);
        match supervisor.current_state() {
            SupervisorState::Backoff { .. } => {} // good
            other => panic!("broken-pipe error must demote to Backoff; got {other:?}"),
        }
        assert!(matches!(
            supervisor.last_reason(),
            Some(RestartReason::BrokenPipe)
        ));
    }

    /// `build_status_payload` with `Ok(true)` → `alive=true`, no
    /// `alive_probe_error` field. Pins the steady-state shape:
    /// successful liveness probe must not poison the payload with
    /// a null sentinel.
    ///
    /// Counterfactual: changing the `Ok` arm to always emit
    /// `alive_probe_error: null` regresses the `is_none()` check.
    #[test]
    fn status_payload_alive_ok_true_omits_probe_error() {
        use crate::supervisor::SupervisorPolicy;
        let queue = BoundedQueue::new(DEFAULT_QUEUE_DEPTH);
        let supervisor = SupervisorPolicy::with_system_clock();
        let mut backend = Clangd::new("/nonexistent/project/root");
        let payload = build_status_payload(&mut backend, &queue, &supervisor, Ok(true));
        assert_eq!(payload["alive"], true);
        assert!(
            payload.get("alive_probe_error").is_none(),
            "Ok path must omit alive_probe_error; got {payload}"
        );
    }

    /// `build_status_payload` with `Ok(false)` → `alive=false`, no
    /// `alive_probe_error`. Pins that an honest "child has exited"
    /// observation does NOT collide with the IO-error surface.
    #[test]
    fn status_payload_alive_ok_false_omits_probe_error() {
        use crate::supervisor::SupervisorPolicy;
        let queue = BoundedQueue::new(DEFAULT_QUEUE_DEPTH);
        let supervisor = SupervisorPolicy::with_system_clock();
        let mut backend = Clangd::new("/nonexistent/project/root");
        let payload = build_status_payload(&mut backend, &queue, &supervisor, Ok(false));
        assert_eq!(payload["alive"], false);
        assert!(
            payload.get("alive_probe_error").is_none(),
            "Ok(false) path must omit alive_probe_error; got {payload}"
        );
    }

    /// `build_status_payload` with `Err(io::Error)` → `alive=false`
    /// AND `alive_probe_error: "io error: <msg>"`. Drives the
    /// LOW-AR2 fix: prior shape was `is_alive().unwrap_or(false)`
    /// which silently swallowed the IO error and surfaced as a
    /// phantom `alive=false`, indistinguishable from a clean exit.
    /// Operators saw "supervisor restarting" with no diagnostic
    /// when the probe itself was broken (EBADF on a moved-out
    /// child handle, EINTR not retried, etc.).
    ///
    /// Counterfactual (mutation probe): reverting the Err arm of
    /// `build_status_payload` to drop `alive_probe_error`
    /// (i.e. `let alive = alive_result.unwrap_or(false);` with
    /// no error surface) makes both assertions below regress —
    /// the field check fails because no field is inserted.
    #[test]
    fn status_payload_alive_probe_error_surfaces_io_failure() {
        use crate::supervisor::SupervisorPolicy;
        let queue = BoundedQueue::new(DEFAULT_QUEUE_DEPTH);
        let supervisor = SupervisorPolicy::with_system_clock();
        let mut backend = Clangd::new("/nonexistent/project/root");
        let alive_result = Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "synthetic try_wait failure",
        ));
        let payload = build_status_payload(&mut backend, &queue, &supervisor, alive_result);
        assert_eq!(
            payload["alive"], false,
            "Err probe must report alive=false (no positive liveness signal)"
        );
        let probe_err = payload["alive_probe_error"]
            .as_str()
            .expect("alive_probe_error must be present + a string when probe errored");
        assert!(
            probe_err.contains("synthetic try_wait failure"),
            "alive_probe_error must carry the underlying IO error message; got {probe_err:?}"
        );
        assert!(
            probe_err.starts_with("io error: "),
            "alive_probe_error must be tagged with the `io error:` prefix; got {probe_err:?}"
        );
    }
}
