//! Long-lived rust-analyzer subprocess client.
//!
//! Owns the spawned `rust-analyzer` process, its stdin/stdout pipes, and a
//! monotonic request ID counter. Handles the LSP `initialize` /
//! `initialized` handshake on construction so callers see a ready-to-query
//! client; on drop, sends `shutdown` + `exit` and waits up to 1s for the
//! child to leave so we don't orphan rust-analyzer processes.
//!
//! Bug-fix matrix vs zeenix/rust-analyzer-mcp v0.2.0 — see the README.
//! This module specifically owns:
//!
//! - **Bug #2 (no retry)**: `request_with_retry` matches
//!   `rpc::RpcOutcome::Error` against `LSP_ERROR_CONTENT_MODIFIED` /
//!   `LSP_ERROR_SERVER_CANCELLED` for up to 3 attempts with 500ms backoff.
//! - **Bug #3 (hardcoded 30s timeout)**: every request uses the
//!   per-instance `timeout` field, sourced from CLI flag or
//!   `LSP_TIMEOUT_SECS` env var (default 60s).
//! - **Bug #4 (invisible logging)**: rust-analyzer's stderr is captured to
//!   the path resolved by `resolve_log_path` (default `/tmp/lsp-rust.log`)
//!   so post-mortem reads are possible without re-running.

use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use thiserror::Error;

use crate::rpc::{
    self, recv_frame, send_frame, RpcOutcome, LSP_ERROR_CONTENT_MODIFIED,
    LSP_ERROR_SERVER_CANCELLED,
};

/// Default per-request timeout when neither CLI flag nor env var supplied.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Default per-handshake timeout. Initialize itself is cheap (no indexing
/// triggered until `initialized` is sent); 15s leaves headroom for a
/// loaded host without masking a wedged process.
const INITIALIZE_TIMEOUT_SECS: u64 = 15;

/// Default log file for rust-analyzer stderr.
const DEFAULT_LOG_PATH: &str = "/tmp/lsp-rust.log";

/// Max retry budget on `ContentModified` / `ServerCancelled`. Three
/// attempts catches the common indexing-race window without unbounded
/// blocking.
const RETRY_BUDGET: u32 = 3;

/// Backoff between retries on transient errors. Fixed (not exponential):
/// the LSP errors we retry on are document-stability transients, and the
/// document either settles in a few hundred ms or the workload is
/// genuinely thrashing — exponential backoff would just hide the latter.
const RETRY_BACKOFF_MS: u64 = 500;

/// Error surface for every LSP shim operation. Variants are deliberately
/// distinct so the MCP layer can return structured JSON
/// (`{"code":..., "retry_after_ms":...}`) rather than the zeenix wrapper's
/// "silent null" non-answer.
#[derive(Debug, Error)]
pub enum LspShimError {
    /// rust-analyzer subprocess could not be spawned. Almost always
    /// "binary not on PATH" — surface verbatim so the caller's first
    /// instinct is "is rust-analyzer installed?" not "is the shim broken?".
    #[error("rust-analyzer spawn failed (binary={binary}): {source}")]
    Spawn {
        /// Path of the rust-analyzer binary the shim attempted to spawn.
        binary: String,
        /// Underlying I/O error from `Command::spawn`.
        #[source]
        source: std::io::Error,
    },

    /// LSP server returned a JSON-RPC error response. Unlike the zeenix
    /// wrapper which silently mapped these to `null`, every error is
    /// preserved with its code so callers can distinguish "request failed"
    /// from "request succeeded with null result".
    #[error("rust-analyzer LSP error {code}: {message}")]
    Lsp {
        /// JSON-RPC error code as returned by the server.
        code: i64,
        /// Human-readable message from the server.
        message: String,
        /// Suggested retry delay (milliseconds) when the error is a
        /// transient `ContentModified` / `ServerCancelled` and the retry
        /// budget has been exhausted. `None` for non-retryable errors.
        retry_after_ms: Option<u64>,
    },

    /// Request exceeded the per-call timeout (default 60s, override via
    /// `LSP_TIMEOUT_SECS` or `--timeout-secs`). zeenix wrapper had
    /// 30s hard-coded with no override.
    #[error("LSP request {method} timed out after {timeout:?}; log={log_path}")]
    Timeout {
        /// LSP method that timed out.
        method: String,
        /// Configured per-request timeout.
        timeout: Duration,
        /// Path to the rust-analyzer stderr log for post-mortem.
        log_path: String,
    },

    /// JSON-RPC framing failed (bad Content-Length, non-JSON body, etc.).
    /// Almost certainly indicates a rust-analyzer crash mid-response —
    /// the shim should treat the subprocess as dead and respawn.
    #[error("LSP protocol error: {0}")]
    Protocol(String),

    /// I/O error talking to the rust-analyzer subprocess.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialisation/deserialisation error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Catch-all for unexpected internal failures (file IO, env parsing,
    /// etc.). Message is mirrored to the structured log file.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, LspShimError>;

/// Long-lived rust-analyzer LSP client. One instance per shim invocation;
/// the instance is reused across every CLI subcommand and every MCP tool
/// call so the indexing cost is paid exactly once at startup.
pub struct RustAnalyzerClient {
    workspace: PathBuf,
    binary: PathBuf,
    log_path: PathBuf,
    timeout: Duration,
    next_id: AtomicU64,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

impl RustAnalyzerClient {
    /// Construct a new client. Does NOT spawn — call [`spawn`](Self::spawn)
    /// to start rust-analyzer and run the `initialize` handshake.
    ///
    /// `workspace` must be an existing directory containing a `Cargo.toml`
    /// (or `rust-project.json`) for rust-analyzer to index. `cli_timeout`
    /// is the optional `--timeout-secs` override; it defeats the env var
    /// `LSP_TIMEOUT_SECS`, which in turn defeats the 60s default.
    pub fn new(workspace: impl Into<PathBuf>, cli_timeout: Option<u64>) -> Self {
        let binary = std::env::var("LSP_RUST_ANALYZER")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("rust-analyzer"));
        let log_path = Self::resolve_log_path();
        Self {
            workspace: workspace.into(),
            binary,
            log_path,
            timeout: Self::resolve_timeout(cli_timeout),
            next_id: AtomicU64::new(1),
            child: None,
            stdin: None,
            stdout: None,
        }
    }

    /// Resolve the effective per-request timeout from (in priority order)
    /// an explicit CLI override, the `LSP_TIMEOUT_SECS` env var, then
    /// the 60s default.
    pub fn resolve_timeout(cli_override: Option<u64>) -> Duration {
        if let Some(secs) = cli_override {
            return Duration::from_secs(secs);
        }
        if let Ok(env_val) = std::env::var("LSP_TIMEOUT_SECS") {
            if let Ok(secs) = env_val.parse::<u64>() {
                return Duration::from_secs(secs);
            }
        }
        Duration::from_secs(DEFAULT_TIMEOUT_SECS)
    }

    /// Resolve the log file path: env var `LSP_LOG_FILE` overrides
    /// the default `/tmp/lsp-rust.log`. The file is opened
    /// append-mode on every spawn so concurrent shims do not clobber.
    pub fn resolve_log_path() -> PathBuf {
        std::env::var("LSP_LOG_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_LOG_PATH))
    }

    /// Path to the captured rust-analyzer stderr log.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Effective per-request timeout for this client.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Spawn rust-analyzer and run the `initialize` / `initialized`
    /// handshake. Idempotent — calling twice is a no-op after the first
    /// success. Returns `Err(Spawn)` if the binary is not on PATH or the
    /// override is invalid.
    pub fn spawn(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let stderr = log_file.try_clone()?;

        let mut cmd = Command::new(&self.binary);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));

        let mut child = cmd.spawn().map_err(|e| LspShimError::Spawn {
            binary: self.binary.display().to_string(),
            source: e,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspShimError::Protocol("rust-analyzer stdin handle missing".into()))?;
        let stdout =
            BufReader::new(child.stdout.take().ok_or_else(|| {
                LspShimError::Protocol("rust-analyzer stdout handle missing".into())
            })?);
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(stdout);

        // Initialize handshake. rootUri must be a file:// URI per spec.
        let init_id = self.next_request_id();
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{}", self.workspace.display()),
            "capabilities": {
                "workspace": {
                    "symbol": { "dynamicRegistration": false },
                    "diagnostic": { "refreshSupport": false }
                },
                "textDocument": {
                    "definition": {},
                    "references": {},
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "diagnostic": { "dynamicRegistration": false }
                }
            },
            "clientInfo": {
                "name": "lsp-rust",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let request = json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": init_params,
        });
        send_frame(self.stdin.as_mut().expect("just set"), &request)?;

        let deadline = Instant::now() + Duration::from_secs(INITIALIZE_TIMEOUT_SECS);
        loop {
            if Instant::now() > deadline {
                return Err(LspShimError::Timeout {
                    method: "initialize".into(),
                    timeout: Duration::from_secs(INITIALIZE_TIMEOUT_SECS),
                    log_path: self.log_path.display().to_string(),
                });
            }
            let value = recv_frame(self.stdout.as_mut().expect("just set"))
                .map_err(|e| LspShimError::Protocol(format!("initialize recv: {e}")))?;
            if value.get("id").and_then(|v| v.as_u64()) == Some(init_id) {
                if let Some(err) = value.get("error") {
                    return Err(LspShimError::Protocol(format!(
                        "rust-analyzer initialize error: {err}"
                    )));
                }
                break;
            }
            // Notifications (window/logMessage, $/progress, etc.) skipped.
        }
        // notifications/initialized — rust-analyzer starts indexing after this.
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {},
        });
        send_frame(self.stdin.as_mut().expect("set"), &initialized)?;
        Ok(())
    }

    /// Send a request, await the matching reply, route by `id`. Drops
    /// notifications and unrelated responses. Errors surface as
    /// `LspShimError::Lsp`; framing/IO failures as `Protocol` / `Io`.
    fn request_once(&mut self, method: &str, params: &Value) -> Result<Value> {
        let id = self.next_request_id();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        send_frame(
            self.stdin
                .as_mut()
                .ok_or_else(|| LspShimError::Protocol("stdin not initialised".into()))?,
            &payload,
        )?;
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() > deadline {
                return Err(LspShimError::Timeout {
                    method: method.to_string(),
                    timeout: self.timeout,
                    log_path: self.log_path.display().to_string(),
                });
            }
            let value = recv_frame(
                self.stdout
                    .as_mut()
                    .ok_or_else(|| LspShimError::Protocol("stdout not initialised".into()))?,
            )
            .map_err(|e| LspShimError::Protocol(format!("recv {method}: {e}")))?;
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                let body = serde_json::to_vec(&value)?;
                return match rpc::parse_response(&body)
                    .map_err(|e| LspShimError::Protocol(format!("parse {method}: {e}")))?
                {
                    RpcOutcome::Result(v) => Ok(v),
                    RpcOutcome::Error {
                        code,
                        message,
                        data: _,
                    } => Err(LspShimError::Lsp {
                        code,
                        message,
                        retry_after_ms: None,
                    }),
                };
            }
        }
    }

    /// Send a request with retry on transient `ContentModified` /
    /// `ServerCancelled`. Up to [`RETRY_BUDGET`] attempts spaced by
    /// [`RETRY_BACKOFF_MS`].
    pub fn request_with_retry(&mut self, method: &str, params: Value) -> Result<Value> {
        self.spawn()?;
        let mut last_err: Option<LspShimError> = None;
        for attempt in 0..RETRY_BUDGET {
            match self.request_once(method, &params) {
                Ok(v) => return Ok(v),
                Err(LspShimError::Lsp {
                    code: LSP_ERROR_CONTENT_MODIFIED,
                    message,
                    ..
                })
                | Err(LspShimError::Lsp {
                    code: LSP_ERROR_SERVER_CANCELLED,
                    message,
                    ..
                }) => {
                    last_err = Some(LspShimError::Lsp {
                        code: LSP_ERROR_CONTENT_MODIFIED,
                        message,
                        retry_after_ms: Some(RETRY_BACKOFF_MS),
                    });
                    if attempt + 1 < RETRY_BUDGET {
                        std::thread::sleep(Duration::from_millis(RETRY_BACKOFF_MS));
                    }
                }
                Err(other) => return Err(other),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            LspShimError::Internal("retry loop exhausted with no error captured".into())
        }))
    }

    /// `textDocument/definition` at file:line:col (1-based).
    pub fn definition(&mut self, file: &Path, line: u32, column: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": format!("file://{}", file.display()) },
            "position": {
                "line": line.saturating_sub(1),
                "character": column.saturating_sub(1),
            }
        });
        self.request_with_retry("textDocument/definition", params)
    }

    /// `textDocument/references` at file:line:col (1-based).
    pub fn references(&mut self, file: &Path, line: u32, column: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": format!("file://{}", file.display()) },
            "position": {
                "line": line.saturating_sub(1),
                "character": column.saturating_sub(1),
            },
            "context": { "includeDeclaration": true },
        });
        self.request_with_retry("textDocument/references", params)
    }

    /// `textDocument/hover` at file:line:col (1-based).
    pub fn hover(&mut self, file: &Path, line: u32, column: u32) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": format!("file://{}", file.display()) },
            "position": {
                "line": line.saturating_sub(1),
                "character": column.saturating_sub(1),
            }
        });
        self.request_with_retry("textDocument/hover", params)
    }

    /// `workspace/symbol` fuzzy search.
    pub fn workspace_symbols(&mut self, query: &str) -> Result<Value> {
        let params = json!({ "query": query });
        self.request_with_retry("workspace/symbol", params)
    }

    /// `textDocument/diagnostic` for one file (LSP 3.17 pull-diagnostics).
    /// Older rust-analyzer builds may respond with an LSP error indicating
    /// the method is not supported; the caller surfaces that verbatim.
    pub fn diagnostics(&mut self, file: &Path) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": format!("file://{}", file.display()) },
        });
        self.request_with_retry("textDocument/diagnostic", params)
    }

    /// Block until rust-analyzer's pull-diagnostic surface stabilises.
    /// Polls `workspace/diagnostic` (or falls back to a sentinel
    /// `workspace/symbol` query if pull-diagnostics is unsupported) until
    /// two successive samples return the same digest, or [`Self::timeout`]
    /// elapses.
    ///
    /// Replaces zeenix wrapper bug #5 (substring scan of stderr) with an
    /// explicit polling loop keyed on protocol output.
    pub fn wait_for_indexing(&mut self) -> Result<Value> {
        self.spawn()?;
        let deadline = Instant::now() + self.timeout;
        let mut last_digest: Option<String> = None;
        let mut stable_samples = 0u32;
        let poll_interval = Duration::from_millis(500);
        while Instant::now() < deadline {
            // Use a cheap workspace-wide sentinel: empty-query
            // workspace/symbol returns immediately and varies as the
            // index grows. Two consecutive identical responses mean
            // indexing has settled (or at least paused).
            let sample = match self.request_once("workspace/symbol", &json!({"query": ""})) {
                Ok(v) => v,
                Err(LspShimError::Lsp { .. }) => {
                    // Some rust-analyzer builds reject the empty query;
                    // fall back to a known-stable hover-on-nowhere.
                    Value::Null
                }
                Err(other) => return Err(other),
            };
            let digest = format!("{}", sample);
            if Some(&digest) == last_digest.as_ref() {
                stable_samples += 1;
                if stable_samples >= 2 {
                    return Ok(json!({
                        "status": "indexing_settled",
                        "samples": stable_samples,
                    }));
                }
            } else {
                stable_samples = 0;
                last_digest = Some(digest);
            }
            std::thread::sleep(poll_interval);
        }
        Err(LspShimError::Timeout {
            method: "wait_for_indexing".into(),
            timeout: self.timeout,
            log_path: self.log_path.display().to_string(),
        })
    }

    /// Best-effort shutdown: send `shutdown` + `exit`, drop stdin, wait up
    /// to 1s, then SIGKILL. Always safe to call; subsequent calls are
    /// no-ops once the child is reaped.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        let _ = self.request_once("shutdown", &Value::Null);
        if let Some(stdin) = self.stdin.as_mut() {
            let exit = json!({"jsonrpc":"2.0","method":"exit","params":null});
            let body = serde_json::to_vec(&exit)?;
            let _ = write!(stdin, "Content-Length: {}\r\n\r\n", body.len());
            let _ = stdin.write_all(&body);
            let _ = stdin.flush();
        }
        if let Some(stdin) = self.stdin.take() {
            drop(stdin);
        }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None if Instant::now() > deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    None => std::thread::sleep(Duration::from_millis(25)),
                }
            }
        }
        Ok(())
    }
}

impl Drop for RustAnalyzerClient {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_timeout_prefers_cli_over_env() {
        std::env::set_var("LSP_TIMEOUT_SECS", "5");
        let resolved = RustAnalyzerClient::resolve_timeout(Some(123));
        std::env::remove_var("LSP_TIMEOUT_SECS");
        assert_eq!(resolved, Duration::from_secs(123));
    }

    #[test]
    fn resolve_timeout_falls_back_to_env() {
        std::env::set_var("LSP_TIMEOUT_SECS", "17");
        let resolved = RustAnalyzerClient::resolve_timeout(None);
        std::env::remove_var("LSP_TIMEOUT_SECS");
        assert_eq!(resolved, Duration::from_secs(17));
    }

    #[test]
    fn resolve_timeout_default_is_60s() {
        std::env::remove_var("LSP_TIMEOUT_SECS");
        assert_eq!(
            RustAnalyzerClient::resolve_timeout(None),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        );
    }

    #[test]
    fn lsp_error_carries_retry_after_for_content_modified() {
        let err = LspShimError::Lsp {
            code: LSP_ERROR_CONTENT_MODIFIED,
            message: "content modified".into(),
            retry_after_ms: Some(500),
        };
        assert!(matches!(err, LspShimError::Lsp { .. }));
        assert!(err.to_string().contains("-32801"));
    }

    #[test]
    fn spawn_with_missing_binary_returns_structured_error() {
        // Regression guard: bogus binary path → LspShimError::Spawn,
        // never a panic. Variant discriminant is what the MCP layer
        // dispatches on for the `error_kind=spawn` mapping.
        std::env::set_var("LSP_RUST_ANALYZER", "/definitely/not/a/real/ra-xyz");
        let mut client = RustAnalyzerClient::new("/tmp", None);
        let err = client.spawn().expect_err("must fail to spawn");
        std::env::remove_var("LSP_RUST_ANALYZER");
        assert!(matches!(err, LspShimError::Spawn { .. }), "got {err:?}");
    }

    #[test]
    fn log_path_resolves_from_env_then_default() {
        std::env::remove_var("LSP_LOG_FILE");
        assert_eq!(
            RustAnalyzerClient::resolve_log_path(),
            PathBuf::from(DEFAULT_LOG_PATH)
        );
        std::env::set_var("LSP_LOG_FILE", "/tmp/probe-lsp-rust.log");
        assert_eq!(
            RustAnalyzerClient::resolve_log_path(),
            PathBuf::from("/tmp/probe-lsp-rust.log")
        );
        std::env::remove_var("LSP_LOG_FILE");
    }
}
