//! clangd-19 backend.
//!
//! Spawns clangd with the same flag set the existing
//! `the upstream `clangd-launcher/start-clangd.sh` script` uses, so the index produced by the
//! background driver is bit-identical to the one this shim consumes.
//!
//! Settings (kept in sync with `the upstream `clangd-launcher/clangd-driver.py` script`):
//!
//! - `--background-index` — populate the persistent index.
//! - `--background-index-priority=background` — yield to interactive
//!   queries; matches the driver's value.
//! - `--pch-storage=disk` — stores PCH on disk; lockstep with
//!   `the upstream `clangd-launcher/clangd-driver.py` script:72` so shim and out-of-band
//!   indexer share build-args fingerprint and reuse `.cache/clangd/`
//!   shards. Without this, shim's clangd treats the indexer's shards
//!   as alien (different fingerprint hash).
//! - `--header-insertion=never` — large C++ project PCH layouts makes auto-insertion
//!   noisy.
//! - `--clang-tidy=false` — tidy adds latency without value for the
//!   verbatim-port use case (we treat upstream sources as read-only).
//! - `-j=$CLANGD_JOBS` — defaults to half of `nproc` (min 2). Override
//!   via env var. project TUs use ~1.5 GB RSS each at peak; half-cpu policy
//!   keeps headroom for cargo/rust-analyzer/test runs to co-execute.
//! - `--compile-commands-dir=<root>` — points at the directory that
//!   contains `compile_commands.json` (or the narrow variant).
//!
//! The narrow-vs-full DB selection happens in
//! [`Clangd::resolve_compile_commands_dir`], which prefers
//! `compile_commands.narrow.json` when present and falls back to the
//! full DB otherwise. This mirrors `clangd-driver.py`'s behaviour so
//! both consumers agree on which TUs are in scope.

use crate::backend::{Hover, Location, LspBackend, Symbol};
use crate::error::{Result, ShimError};
use crate::jsonrpc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// Name of the environment variable that lists header paths to
/// seed-didOpen after the LSP `initialize` handshake.
///
/// Format: comma-separated absolute paths. Empty / unset disables
/// seed-didOpen entirely.
pub const SEED_HEADERS_ENV: &str = "LSP_CPP_SEED_HEADERS";

/// Read [`SEED_HEADERS_ENV`] and return the configured list of header
/// paths to send `textDocument/didOpen` for after `initialize`.
///
/// Each entry is treated as an absolute path. Empty entries (the
/// "trim then filter" pass below) are dropped so `LSP_CPP_SEED_HEADERS=,`
/// behaves the same as unset.
///
/// ## What seed-didOpen does
///
/// Sending `textDocument/didOpen` for a header forces clangd to parse
/// its translation-unit closure right away and populate the in-memory
/// symbol table, so the first `workspace/symbol` query resolves those
/// symbols without waiting for `--background-index` shards to land on
/// disk. Useful when there is a known set of high-traffic headers
/// callers will hit immediately after spawn.
///
/// Default is empty. Each header parse can take several seconds on a
/// cold cache, so callers should keep the list short.
pub fn seed_headers_from_env() -> Vec<PathBuf> {
    std::env::var(SEED_HEADERS_ENV)
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// v0.3 Patch A: curated default seed-headers list for an Unreal Engine
/// project root. Triggers when the project root path contains the
/// segment `UnrealEngine` (i.e. the operator is pointing the shim at
/// `vendor/UnrealEngine` or similar) AND `LSP_CPP_SEED_HEADERS` is
/// unset. This is the Cropout-port surface — the headers Claude/codex
/// queries pre-port for `AActor`, `USceneComponent`, `UPrimitiveComponent`,
/// the gameplay statics library, sound/material asset wrappers, the
/// data-table machinery, and the AI controller bindings.
///
/// Returning a curated default avoids the prior failure mode where a
/// fresh shim against `vendor/UnrealEngine` returned `[]` for every
/// `workspace_symbol` query because the operator forgot to set
/// `LSP_CPP_SEED_HEADERS`, the on-disk shards hadn't loaded yet, and
/// the in-memory index was empty. Each header parse can take a few
/// seconds on a cold cache, so the list is intentionally short.
pub(crate) fn ue_default_seed_headers(project_root: &Path) -> Vec<PathBuf> {
    const UE_SEED_RELATIVES: &[&str] = &[
        "Engine/Source/Runtime/Engine/Classes/GameFramework/Actor.h",
        "Engine/Source/Runtime/Engine/Classes/Components/SceneComponent.h",
        "Engine/Source/Runtime/Engine/Classes/Components/PrimitiveComponent.h",
        "Engine/Source/Runtime/Engine/Classes/Components/ActorComponent.h",
        "Engine/Source/Runtime/Engine/Classes/Kismet/GameplayStatics.h",
        "Engine/Source/Runtime/Engine/Classes/Sound/SoundBase.h",
        "Engine/Source/Runtime/Engine/Classes/Materials/MaterialInstance.h",
        "Engine/Source/Runtime/Engine/Classes/Materials/MaterialInstanceDynamic.h",
        "Engine/Source/Runtime/Engine/Classes/Engine/DataTable.h",
        "Engine/Source/Runtime/Engine/Classes/Kismet/DataTableFunctionLibrary.h",
        "Engine/Source/Runtime/AIModule/Classes/AIController.h",
        "Engine/Source/Runtime/InputCore/Classes/InputCoreTypes.h",
    ];
    UE_SEED_RELATIVES
        .iter()
        .map(|rel| project_root.join(rel))
        .collect()
}

/// Resolve the effective seed-headers list for a given project root.
/// Order of precedence:
///
/// 1. `LSP_CPP_SEED_HEADERS` env var (verbatim absolute paths) when set
///    and non-empty.
/// 2. UE curated default ([`ue_default_seed_headers`]) when the project
///    root path contains the `UnrealEngine` segment.
/// 3. Empty list (no seeding) otherwise.
pub(crate) fn effective_seed_headers(project_root: &Path) -> Vec<PathBuf> {
    let from_env = seed_headers_from_env();
    if !from_env.is_empty() {
        return from_env;
    }
    let root_str = project_root.to_string_lossy();
    if root_str.contains("UnrealEngine") {
        return ue_default_seed_headers(project_root);
    }
    Vec::new()
}

/// Default request timeout. clangd can stall on a single TU for 30 s+
/// during cold-cache PCH parses; 60 s is the same value
/// `clangd-driver.py` uses for its heartbeat cadence.
const DEFAULT_REQUEST_TIMEOUT_S: u64 = 60;

/// Default initialize timeout. Initialize is cheap (no indexing yet);
/// 10 s is generous.
const DEFAULT_INITIALIZE_TIMEOUT_S: u64 = 10;

/// Indexing mode for the backing clangd instance.
///
/// clangd has two index sources: the live `--background-index` worker
/// (writes into `.cache/clangd/index/` as it parses TUs) and a
/// pre-built serialized index supplied via `--index-file=<path>` (the
/// output of `clangd-indexer`, a separate binary built from the same
/// clang-tools-extra source). Production setups for very large
/// codebases (LLVM, Chromium, large monorepos) pre-build the index
/// out-of-band so the LSP server is fast on first query.
///
/// This crate supports three modes so callers can trade cold-start cost
/// against query coverage:
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexMode {
    /// Live narrow indexing. clangd is told about
    /// `compile_commands.narrow.json` (or full DB if narrow absent)
    /// and runs `--background-index` against it. Cold-start is
    /// minutes, queries cover only the chosen subset of TUs.
    ///
    /// Default. Matches `the upstream `clangd-launcher/clangd-driver.py` script` behaviour.
    Narrow,
    /// Pre-built full project index, read-only. clangd is launched with
    /// `--index-file=<path>` only; `--background-index` is suppressed.
    /// First-query latency is sub-second because no PCH parses run,
    /// but anything not in the pre-built index is invisible.
    ///
    /// Index built out-of-band by `clangd-indexer` (separate
    /// binary). The build subcommand for this lives on follow-up
    /// a follow-up branch; this mode is
    /// selectable today so a pre-existing index file can be consumed.
    Full {
        /// Absolute path to the serialized `index.idx` produced by
        /// `clangd-indexer`. Default: `~/.cache/lsp-cpp-full-index/index.idx`.
        index_file: PathBuf,
    },
    /// Hybrid: pre-built full project index seeds clangd's symbol table,
    /// `--background-index` fills gaps for TUs the indexer skipped or
    /// for files the developer is actively editing. Recommended
    /// production setting once the indexer subcommand lands.
    Hybrid {
        /// Path to the pre-built index, same default as `Full`.
        index_file: PathBuf,
    },
}

impl IndexMode {
    /// Resolve from `LSP_CPP_INDEX_MODE` env var.
    /// Recognised values: `narrow` (default), `full`, `hybrid`.
    /// `full` and `hybrid` honour `LSP_CPP_INDEX_FILE` for the
    /// path; default is `$HOME/.cache/lsp-cpp-full-index/index.idx`.
    pub fn from_env() -> Self {
        let mode = std::env::var("LSP_CPP_INDEX_MODE").unwrap_or_default();
        let index_file = || -> PathBuf {
            std::env::var("LSP_CPP_INDEX_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                    PathBuf::from(home).join(".cache/lsp-cpp-full-index/index.idx")
                })
        };
        match mode.as_str() {
            "full" => IndexMode::Full {
                index_file: index_file(),
            },
            "hybrid" => IndexMode::Hybrid {
                index_file: index_file(),
            },
            _ => IndexMode::Narrow,
        }
    }
}

/// Default cap on the number of TUs `build_full_index` opens in a
/// single invocation. Large monorepos can have 100k+ TUs in `compile_commands.json`;
/// indexing all of them is hours of wall time. The cap lets callers
/// pre-warm a useful subset out-of-band without committing to a full
/// run, and lets the integration test exercise the path with a small
/// `max_tus` (50). Override via the `max_tus` MCP argument or the
/// CLI's `--max-tus` flag.
pub const BUILD_FULL_INDEX_DEFAULT_MAX_TUS: usize = 5000;

/// How long `build_full_index` waits, after the last `didOpen` is
/// queued, for the on-disk shard count to stop growing before declaring
/// the indexing run quiescent. Mirrors the heuristic
/// `clangd-driver.py` uses (it sleeps 20 s between batches and never
/// explicitly waits for "done"); shard-count quiescence is the more
/// faithful signal because shards landing in
/// `.cache/clangd/index/` are the actual artefact callers consume.
const BUILD_FULL_INDEX_QUIESCENCE_SECS: u64 = 20;

/// Hard ceiling on the wall-clock spent waiting for shards to settle
/// after the last `didOpen`. Keeps a stuck clangd from holding the
/// MCP request open forever; the `wall_seconds` field in the report
/// surfaces whether the cap fired.
const BUILD_FULL_INDEX_MAX_DRAIN_SECS: u64 = 600;

/// Per-batch size for `didOpen` notifications. clangd's worker queue
/// grows unbounded if we feed it the entire compile_commands at once;
/// `clangd-driver.py` uses 32 with a 20 s sleep between batches and
/// that holds memory steady on a 4-core container. Same number here.
const BUILD_FULL_INDEX_BATCH: usize = 32;

/// JSON report returned by [`Clangd::build_full_index`] (and surfaced
/// verbatim through the `mcp__lsp-cpp__build_full_index` MCP tool).
///
/// Matches the schema in the dispatch prompt:
/// - `tus_opened` — how many `textDocument/didOpen` notifications were
///   actually sent to clangd.
/// - `skipped_io_errors` — entries the driver could not read from disk
///   (UHT-generated TUs that no longer exist, permission errors, …);
///   `clangd-driver.py` falls back to empty text in that case but we
///   skip outright so the count is informative.
/// - `wall_seconds` — total wall time including the post-feed
///   shard-quiescence wait.
/// - `shards_on_disk_after` — count of `*.idx` files in
///   `.cache/clangd/index/` after the run.
/// - `shards_size_bytes` — `du -sb` equivalent over the same dir.
/// - `cap_fired` — true if the drain-quiescence loop hit
///   [`BUILD_FULL_INDEX_MAX_DRAIN_SECS`] before clangd's
///   shard-write rate quiesced. Distinguishes the "ran to clean
///   end" case from the "we ran out of drain budget" case;
///   without this field callers must infer it from
///   `wall_seconds >= BUILD_FULL_INDEX_MAX_DRAIN_SECS + feed_time`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildFullIndexReport {
    /// Count of `textDocument/didOpen` notifications successfully sent.
    pub tus_opened: usize,
    /// Count of compile_commands entries skipped because the source
    /// file could not be read from disk.
    pub skipped_io_errors: usize,
    /// Total wall seconds, including post-feed quiescence drain.
    pub wall_seconds: f64,
    /// Count of `*.idx` shards in `.cache/clangd/index/` at end of run.
    pub shards_on_disk_after: usize,
    /// Total bytes of `*.idx` shards in `.cache/clangd/index/`.
    pub shards_size_bytes: u64,
    /// True if drain quiescence loop hit
    /// [`BUILD_FULL_INDEX_MAX_DRAIN_SECS`] before clangd quiesced.
    /// False if drain loop exited cleanly via shard-rate stability.
    pub cap_fired: bool,
}

/// clangd backend.
pub struct Clangd {
    project_root: PathBuf,
    clangd_bin: PathBuf,
    log_path: PathBuf,
    jobs: u32,
    request_timeout_s: u64,
    initialize_timeout_s: u64,
    index_mode: IndexMode,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    next_id: AtomicI64,
    /// Last observed exit status from `is_alive()`. Plumbed into the
    /// supervisor's `RestartReason` when a request fails on a dead
    /// child. `None` until the first observed exit.
    last_exit_status: Option<String>,
}

/// Render a `std::process::ExitStatus` into a stable `code=N` /
/// `signal=N` string for the supervisor's status RPC.
fn format_exit_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("code={code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal={sig}");
        }
    }
    "code=?".to_string()
}

/// Default `clangd -j=N` worker count: half of available CPUs (min 2).
///
/// Half-cpu policy: clangd's parsing loop is largely independent per
/// translation unit but uses peak ~1.5 GB RSS per worker on project TU. Half
/// of nproc keeps headroom for cargo/rust-analyzer/test runs to
/// co-execute without paging on a 16-32 cpu dev box. Override via
/// `CLANGD_JOBS=N` env var.
fn default_clangd_jobs() -> u32 {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u32;
    (cpus / 2).max(2)
}

impl Clangd {
    /// Build a new clangd backend rooted at `project_root`.
    ///
    /// `project_root` is the directory containing `compile_commands.json`
    /// (or the narrow variant). For this shim's typical use case that is normally
    /// `/path/to/project`.
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let clangd_bin = std::env::var("CLANGD_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("clangd-19"));
        let log_path = std::env::var("CLANGD_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/clangd.log"));
        let jobs = std::env::var("CLANGD_JOBS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(default_clangd_jobs);
        Self {
            project_root: project_root.into(),
            clangd_bin,
            log_path,
            jobs,
            request_timeout_s: DEFAULT_REQUEST_TIMEOUT_S,
            initialize_timeout_s: DEFAULT_INITIALIZE_TIMEOUT_S,
            index_mode: IndexMode::from_env(),
            child: None,
            stdin: None,
            stdout: None,
            next_id: AtomicI64::new(1),
            last_exit_status: None,
        }
    }

    /// Override the index mode for this backend. Builder-style; call
    /// before `spawn`.
    pub fn with_index_mode(mut self, mode: IndexMode) -> Self {
        self.index_mode = mode;
        self
    }

    /// PID of the live clangd subprocess, if any. `None` when no
    /// child has been spawned yet, or after `shutdown()` reaped it.
    /// Used by the supervisor's `lsp_cpp_status` MCP tool.
    pub(crate) fn clangd_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Best-effort liveness probe: returns `Ok(true)` if the child is
    /// still running, `Ok(false)` if it has exited (and reaps the
    /// zombie), `Err(_)` if `try_wait()` itself failed. Mirrors
    /// `Child::try_wait` semantics — does NOT block. Called by the
    /// supervisor at every MCP request boundary so a zombified
    /// clangd produces a structured `RestartReason::ChildExited`
    /// rather than a downstream `Io(broken pipe)`. The matching
    /// `last_exit_status` accessor surfaces the exit code or signal
    /// for the supervisor's `RestartReason` classification.
    pub(crate) fn is_alive(&mut self) -> std::io::Result<bool> {
        match self.child.as_mut() {
            Some(child) => match child.try_wait()? {
                None => Ok(true),
                Some(status) => {
                    // Reap the zombie + drop the now-stale stdio
                    // handles so the next request triggers a fresh
                    // spawn through `ensure_spawned()`.
                    self.last_exit_status = Some(format_exit_status(&status));
                    self.child = None;
                    self.stdin = None;
                    self.stdout = None;
                    Ok(false)
                }
            },
            None => Ok(false),
        }
    }

    /// Most recent exit status observed by `is_alive()`. Format:
    /// `code=N` for normal exits, `signal=N` for signal kills,
    /// `code=?` if the platform did not provide either.
    pub(crate) fn last_exit_status(&self) -> Option<&str> {
        self.last_exit_status.as_deref()
    }

    /// Locate the directory clangd should be told contains the
    /// compile_commands. Prefers `compile_commands.narrow.json` next
    /// to the project root, falls back to `compile_commands.json`,
    /// returns `NoCompileCommands` if neither exists.
    pub fn resolve_compile_commands_dir(root: &Path) -> Result<PathBuf> {
        let narrow = root.join("compile_commands.narrow.json");
        let full = root.join("compile_commands.json");
        if narrow.exists() || full.exists() {
            // clangd takes a directory, not a file, so the path itself
            // is the directory either way.
            Ok(root.to_path_buf())
        } else {
            Err(ShimError::NoCompileCommands {
                root: root.display().to_string(),
            })
        }
    }

    fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn ensure_spawned(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        self.spawn()
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.ensure_spawned()?;
        let id = self.next_request_id();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ShimError::Protocol("clangd stdin not initialised".into()))?;
        jsonrpc::send(stdin, &payload)?;

        let timeout_s = self.request_timeout_for_method(method);
        let log_path_str = self.log_path.display().to_string();
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        // Re-borrow stdout for the read loop. Cannot touch &mut self
        // again until the loop drops this borrow, so timeout
        // classification (which needs &mut self.child for try_wait) is
        // delegated to a helper called AFTER we break out of the loop.
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| ShimError::Protocol("clangd stdout not initialised".into()))?;
        let mut timed_out = false;
        let result_or_err: Result<Value> = loop {
            if Instant::now() > deadline {
                timed_out = true;
                // Break with a placeholder; classify_timeout below
                // refines this into ClangdBusy or ClangdExited once the
                // &mut self.stdout borrow drops.
                break Err(ShimError::RequestTimeout {
                    method: method.to_string(),
                    timeout_s,
                    log_path: log_path_str.clone(),
                });
            }
            // recv blocks until the next framed message; clangd always
            // responds in order so the next message keyed to `id` is
            // ours. Notifications (window/logMessage etc.) are dropped.
            let value = match jsonrpc::recv(&mut *stdout) {
                Ok(v) => v,
                Err(e) => break Err(e),
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = value.get("error") {
                    break Err(ShimError::Protocol(format!(
                        "clangd error response to {method}: {err}"
                    )));
                }
                break Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            // Otherwise a notification (no id) or some other request id
            // — keep reading.
        };
        // The &mut self.stdout borrow ends here; classify_timeout can
        // probe self.child via &mut self again.
        if timed_out {
            return Err(self.classify_timeout(method, timeout_s));
        }
        result_or_err
    }

    /// Per-method timeout. `workspace/symbol` is intentionally shorter
    /// than the position queries because workspace search is bounded
    /// by index lookup (fast on a hot index, only slow during initial
    /// build) — a slow workspace_symbol call is overwhelmingly likely
    /// to be "wrapper queue stuck" rather than "this query is genuinely
    /// expensive". Position queries (`textDocument/definition`,
    /// `textDocument/references`, `textDocument/hover`) get the full
    /// 60 s because cold-PCH parses on UMG-class TUs can run 30 s+.
    fn request_timeout_for_method(&self, method: &str) -> u64 {
        match method {
            // v0.3 Patch A: bumped 30→90s. Cold UE-engine workspace/symbol
            // queries against a project that hasn't yet finished the
            // seed-didOpen TU-closure parses can take 60s+ before the
            // in-memory symbol table is populated; the prior 30s ceiling
            // produced spurious ClangdBusy errors on the first query
            // even when seed-didOpen was making forward progress.
            "workspace/symbol" => self.request_timeout_s.max(90),
            _ => self.request_timeout_s,
        }
    }

    /// Decide whether a fired timeout is a busy clangd (still alive,
    /// just slow) or a dead clangd (subprocess exited — broken-pipe
    /// surface). Probes `child.try_wait()` non-blockingly: `Ok(None)`
    /// means alive → [`ShimError::ClangdBusy`]; `Ok(Some(status))`
    /// means exited → [`ShimError::ClangdExited`]; `Err(_)` (rare —
    /// only on EINVAL/EINTR-class kernel-level failures) is treated as
    /// alive because we can't prove death without a `wait()` return
    /// value.
    ///
    /// `current_status` is a coarse tag derived from the most-recent
    /// log heartbeat in `self.log_path`. Today the field is filled with
    /// "unknown" — log-heartbeat parsing is deferred to the
    /// heartbeat-progress successor branch
    /// (`lsp-cpp-starvation-heartbeat`), which will replace this
    /// stub with a real tail-and-extract. Wiring the field today makes
    /// it load-bearing for that successor branch's commit; without it,
    /// the successor would have to widen the `ClangdBusy` variant
    /// shape.
    fn classify_timeout(&mut self, method: &str, timeout_s: u64) -> ShimError {
        let log_path = self.log_path.display().to_string();
        let alive = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_status)) => false,
                Err(_) => true,
            },
            None => false,
        };
        if alive {
            ShimError::ClangdBusy {
                method: method.to_string(),
                timeout_s,
                current_status: String::new(),
                log_path,
            }
        } else {
            // Subprocess exited — surface as ClangdExited so the caller
            // (or the auto-resume a follow-up branch's supervisor) can
            // restart the wrapper. This is the broken-pipe taxonomy
            // boundary: ClangdExited replaces what previously surfaced
            // as a generic RequestTimeout when the subprocess had
            // already crashed.
            ShimError::ClangdExited {
                status: "exited_during_request".to_string(),
                log_path,
            }
        }
    }

    /// Send `textDocument/didOpen` for every header listed in
    /// [`SEED_HEADERS_ENV`] so clangd's in-memory index is warm for
    /// those symbols by the time the first query lands. Best-effort
    /// per file — a missing or unreadable header is logged to
    /// `self.log_path` and skipped, never fails the whole spawn. Runs
    /// synchronously inside `spawn()` after the `initialized`
    /// notification so the next caller (CLI or MCP `tools/call`) sees
    /// a primed clangd. Note that didOpen is itself a notification —
    /// clangd parses TUs asynchronously, so callers still want a brief
    /// settle window before the first query.
    ///
    /// If the env var is unset or empty this is a no-op. Each entry
    /// MUST be an absolute path; the path is opened verbatim.
    ///
    /// Infallible by construction: every per-file error path logs +
    /// `continue`s, and the loop never propagates `notify` errors out.
    /// Returning `()` (not `Result<()>`) so the `spawn()` call site
    /// can't `?`-propagate a phantom error path that doesn't exist.
    fn seed_didopen(&mut self) {
        // Clone outside the loop to release the borrow on self before
        // we call `self.notify` / `log_seed_event` (both take &mut self).
        let log_path = self.log_path.clone();
        // v0.3 Patch A: use effective_seed_headers so a UE project root
        // with no LSP_CPP_SEED_HEADERS set still gets the curated
        // Cropout-port surface seeded.
        let headers = effective_seed_headers(&self.project_root);
        for abs in headers {
            let text = match std::fs::read_to_string(&abs) {
                Ok(t) => t,
                Err(e) => {
                    log_seed_event(
                        &log_path,
                        &format!(
                            "lsp-cpp seed-didOpen: skip {} (read failed: {})",
                            abs.display(),
                            e
                        ),
                    );
                    continue;
                }
            };
            let uri = format!("file://{}", abs.display());
            let params = json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "cpp",
                    "version": 1,
                    "text": text,
                }
            });
            match self.notify("textDocument/didOpen", params) {
                Ok(()) => log_seed_event(
                    &log_path,
                    &format!("lsp-cpp seed-didOpen: opened {}", abs.display()),
                ),
                Err(e) => log_seed_event(
                    &log_path,
                    &format!(
                        "lsp-cpp seed-didOpen: notify failed for {}: {}",
                        abs.display(),
                        e
                    ),
                ),
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.ensure_spawned()?;
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ShimError::Protocol("clangd stdin not initialised".into()))?;
        jsonrpc::send(stdin, &payload)
    }

    /// Pre-warm clangd's persistent index by feeding the first
    /// `max_tus` TUs from the project's `compile_commands.json`. Ports
    /// `the upstream `clangd-launcher/clangd-driver.py` script` to Rust and runs in-process
    /// inside the MCP shim instead of as a separate Python wrapper.
    ///
    /// Strategy:
    ///
    /// 1. Spawn clangd if not already running. The Narrow / Hybrid
    ///    index modes pass `--background-index`, which is the load-
    ///    bearing flag for this method — without it clangd does not
    ///    write shards to `.cache/clangd/index/` at all and the call
    ///    has no observable effect. Full mode (read-only against a
    ///    pre-built index) is rejected with `Protocol(...)` because
    ///    "build the index" with the indexer disabled is a config
    ///    error worth surfacing rather than silently succeeding.
    /// 2. Locate the compile DB. Prefers `compile_commands.json`
    ///    (full DB) since the tool's job is to expand index coverage
    ///    beyond the narrow subset; falls back to
    ///    `compile_commands.narrow.json` only if the full DB is
    ///    absent.
    /// 3. Iterate up to `max_tus` unique-by-path entries. For each:
    ///    read source from disk (skip + count on I/O error), send a
    ///    `textDocument/didOpen` notification (no response expected).
    ///    Batch in groups of [`BUILD_FULL_INDEX_BATCH`] with
    ///    `didClose` between batches so clangd evicts ASTs but keeps
    ///    the persistent index entries it built — same memory-bounded
    ///    pattern the Python driver uses.
    /// 4. After the last `didOpen`, drain the indexing queue by
    ///    polling the on-disk shard count every 2 s. When the count
    ///    hasn't changed for [`BUILD_FULL_INDEX_QUIESCENCE_SECS`]
    ///    seconds (or the [`BUILD_FULL_INDEX_MAX_DRAIN_SECS`] cap
    ///    fires), declare the run done.
    /// 5. Snapshot final shard count + total `.idx` byte size, return
    ///    [`BuildFullIndexReport`].
    ///
    /// Progress is logged to `self.log_path` (default
    /// `/tmp/clangd.log` — same file the existing CLI tools tail)
    /// so the caller can `tail -f` for liveness while the call is in
    /// flight.
    ///
    /// **Caveat — `--background-index` ↔ shard persistence.** Without
    /// the `--background-index` argv flag (added on a follow-up branch
    /// `lsp-cpp-bgindex-flag`), shards land in the project
    /// `.cache/clangd/index/` while the clangd process is alive but
    /// are not re-loaded into memory across shim restarts; the next
    /// MCP session has to re-read them from disk on first query.
    /// That branch should land before this one in any merge sequence
    /// where shard reuse matters.
    pub fn build_full_index(&mut self, max_tus: usize) -> Result<BuildFullIndexReport> {
        if matches!(self.index_mode, IndexMode::Full { .. }) {
            return Err(ShimError::Protocol(
                "build_full_index requires --background-index, but the shim was launched in \
                 IndexMode::Full (read-only against a pre-built index file). Restart with \
                 LSP_CPP_INDEX_MODE=narrow or hybrid."
                    .into(),
            ));
        }
        self.ensure_spawned()?;

        let started = Instant::now();
        let cdb_path = pick_compile_commands_path(&self.project_root)?;
        let entries = read_compile_commands(&cdb_path)?;
        let total = entries.len();
        let cap = max_tus.min(total);
        log_line(
            &self.log_path,
            &format!(
                "[build_full_index] cdb={} total_tus={} cap={} batch={}",
                cdb_path.display(),
                total,
                cap,
                BUILD_FULL_INDEX_BATCH
            ),
        );

        let index_dir = self.project_root.join(".cache/clangd/index");
        let shards_before = count_shards(&index_dir);
        log_line(
            &self.log_path,
            &format!("[build_full_index] shards_before={shards_before}"),
        );

        let mut tus_opened = 0usize;
        let mut skipped_io_errors = 0usize;
        let mut open_uris: Vec<String> = Vec::with_capacity(BUILD_FULL_INDEX_BATCH);

        for (batch_idx, chunk) in entries[..cap].chunks(BUILD_FULL_INDEX_BATCH).enumerate() {
            // Close the previous batch so clangd evicts the AST cache
            // while keeping the index entries already serialised.
            {
                let stdin = self.stdin.as_mut().ok_or_else(|| {
                    ShimError::Protocol("clangd stdin not initialised".into())
                })?;
                for uri in open_uris.drain(..) {
                    let _ = jsonrpc::send(
                        &mut *stdin,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didClose",
                            "params": { "textDocument": { "uri": uri } },
                        }),
                    );
                }

                for path in chunk {
                    let text = match std::fs::read_to_string(path) {
                        Ok(s) => s,
                        Err(_) => {
                            skipped_io_errors += 1;
                            // clangd-driver.py opens these with empty
                            // text; we skip outright so the report's
                            // skipped_io_errors counter is meaningful.
                            continue;
                        }
                    };
                    let uri = format!("file://{path}");
                    jsonrpc::send(
                        &mut *stdin,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didOpen",
                            "params": {
                                "textDocument": {
                                    "uri": uri,
                                    "languageId": "cpp",
                                    "version": 1,
                                    "text": text,
                                }
                            },
                        }),
                    )?;
                    open_uris.push(uri);
                    tus_opened += 1;
                }
            }

            // v0.3 lifetime fix: drain pending stdout notifications
            // INLINE between batches so clangd's stdout pipe buffer
            // never fills (which would deadlock our next stdin write
            // because clangd would block on its stdout-write before
            // reading more stdin). Replaces the prior thread::spawn +
            // self.stdout.take() pattern that left the reader thread
            // owning stdout and forced a SIGKILL of clangd at the end
            // of every build_full_index call.
            drain_pending_stdout(self.stdout.as_mut());

            log_line(
                &self.log_path,
                &format!(
                    "[build_full_index] batch={} tus_opened={} skipped_io_errors={} elapsed_s={:.1}",
                    batch_idx,
                    tus_opened,
                    skipped_io_errors,
                    started.elapsed().as_secs_f64()
                ),
            );
        }

        // Close the final batch.
        {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| ShimError::Protocol("clangd stdin not initialised".into()))?;
            for uri in open_uris.drain(..) {
                let _ = jsonrpc::send(
                    &mut *stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/didClose",
                        "params": { "textDocument": { "uri": uri } },
                    }),
                );
            }
        }

        // Shard-count quiescence drain. Each iteration also drains any
        // stdout notifications clangd emitted since the last iteration
        // (mostly $/progress on the backgroundIndex token) so the
        // stdout pipe buffer does not fill while we sleep.
        let drain_started = Instant::now();
        let mut last_count = count_shards(&index_dir);
        let mut last_change = Instant::now();
        let mut cap_fired = false;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            drain_pending_stdout(self.stdout.as_mut());
            let now_count = count_shards(&index_dir);
            if now_count != last_count {
                last_change = Instant::now();
                last_count = now_count;
                log_line(
                    &self.log_path,
                    &format!(
                        "[build_full_index] draining shards={} elapsed_s={:.1}",
                        now_count,
                        started.elapsed().as_secs_f64()
                    ),
                );
            }
            if last_change.elapsed() >= Duration::from_secs(BUILD_FULL_INDEX_QUIESCENCE_SECS) {
                log_line(
                    &self.log_path,
                    &format!(
                        "[build_full_index] quiescent shards={} drain_s={:.1}",
                        now_count,
                        drain_started.elapsed().as_secs_f64()
                    ),
                );
                break;
            }
            if drain_started.elapsed() >= Duration::from_secs(BUILD_FULL_INDEX_MAX_DRAIN_SECS) {
                cap_fired = true;
                log_line(
                    &self.log_path,
                    &format!(
                        "[build_full_index] drain_cap_hit shards={} drain_s={:.1}",
                        now_count,
                        drain_started.elapsed().as_secs_f64()
                    ),
                );
                break;
            }
        }

        let shards_on_disk_after = count_shards(&index_dir);
        let shards_size_bytes = sum_shard_bytes(&index_dir);
        let wall_seconds = started.elapsed().as_secs_f64();
        let report = BuildFullIndexReport {
            tus_opened,
            skipped_io_errors,
            wall_seconds,
            shards_on_disk_after,
            shards_size_bytes,
            cap_fired,
        };
        log_line(
            &self.log_path,
            &format!(
                "[build_full_index] done {}",
                serde_json::to_string(&report).unwrap_or_default()
            ),
        );
        // v0.3 lifetime fix: do NOT shut down clangd here. The prior
        // implementation sent `exit` + dropped stdin + SIGKILLed the
        // child to make a detached reader thread (which had taken
        // ownership of self.stdout) unblock. Inline drain replaces the
        // detached thread, so stdout is still owned by self and
        // clangd stays alive for subsequent workspace_symbol /
        // definition / hover queries — which is the whole point of
        // pre-warming the index. The matching integration test
        // (build_full_index_preserves_child) asserts the PID is
        // unchanged across this call.
        Ok(report)
    }
}

/// Drain any pending notifications on clangd's stdout pipe without
/// blocking. Called by [`Clangd::build_full_index`] between batches and
/// during the shard-quiescence drain so a slow consumer of stdout
/// (this process, while it is sleeping or feeding the next batch)
/// never lets clangd block on a full stdout pipe.
///
/// Strategy: use `libc::poll` with a 0 ms timeout against the raw fd
/// underlying the `BufReader<ChildStdout>`. When the fd reports
/// `POLLIN`, parse one frame via [`jsonrpc::recv`] and discard the
/// content. Repeat until poll says no data is available.
///
/// **`BufReader` interaction.** `BufReader` has its own internal byte
/// buffer; if a previous `recv` left bytes in the buffer they will be
/// served by the next `recv` without a syscall, but `poll` operates on
/// the underlying fd and won't see those bytes. We therefore
/// short-circuit when `BufReader::buffer()` is non-empty: parse from
/// the buffer first, then fall through to `poll` for fresh data.
///
/// All errors are swallowed — drain is best-effort. A framing error
/// or EOF indicates clangd has closed, which the next request through
/// `Clangd::request` will surface as a structured `ClangdExited` via
/// the existing `is_alive()` probe.
fn drain_pending_stdout(stdout: Option<&mut BufReader<ChildStdout>>) {
    use std::os::unix::io::AsRawFd;
    let stdout = match stdout {
        Some(s) => s,
        None => return,
    };
    // Bound iterations to keep this from running forever if clangd is
    // emitting notifications faster than we can drain (very unlikely
    // in practice — $/progress messages are throttled by clangd).
    for _ in 0..1024 {
        let buf_has_data = !stdout.buffer().is_empty();
        let fd_ready = if buf_has_data {
            true
        } else {
            poll_fd_ready(stdout.get_ref().as_raw_fd(), 0)
        };
        if !fd_ready {
            return;
        }
        match jsonrpc::recv(&mut *stdout) {
            Ok(_msg) => continue,
            Err(_) => return,
        }
    }
}

/// Non-blocking readiness check on a raw fd. Returns `true` if the fd
/// is currently readable (`POLLIN`), `false` if the timeout expires
/// without data or `poll` itself errored. Wraps the libc `poll` syscall
/// directly to avoid pulling in a heavier async runtime; the surface
/// here is small enough that the unsafe block is contained.
fn poll_fd_ready(fd: std::os::unix::io::RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd is a valid &mut to a single pollfd; nfds=1 matches.
    // libc::poll is a thin syscall wrapper and is async-signal-safe.
    let rc = unsafe { libc::poll(&mut pfd as *mut _, 1, timeout_ms) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Pick the compile-commands JSON path. Prefers the full DB so the
/// tool's name (`build_full_index`) matches its behaviour; falls back
/// to the narrow DB so the function still does *something* useful on
/// hosts that only generated the narrow subset.
pub(crate) fn pick_compile_commands_path(root: &Path) -> Result<PathBuf> {
    let full = root.join("compile_commands.json");
    let narrow = root.join("compile_commands.narrow.json");
    if full.exists() {
        Ok(full)
    } else if narrow.exists() {
        Ok(narrow)
    } else {
        Err(ShimError::NoCompileCommands {
            root: root.display().to_string(),
        })
    }
}

/// Read `compile_commands.json` and extract unique `file` paths in
/// declaration order. Duplicate entries (same TU listed for multiple
/// build configurations) are deduped — the first occurrence wins.
///
/// **Memory note** (Carmack flagged in build_full_index review
/// a83da932d14666385): this fully buffers the entire JSON file into
/// a `Vec<u8>` and then materializes a `serde_json::Value` tree. For
/// a large compile_commands.json (~300-500 MB on disk) the peak
/// in-process RSS during this call is ~1-1.5 GB. At default
/// `BUILD_FULL_INDEX_DEFAULT_MAX_TUS = 5000` this is the dominant
/// allocation in `build_full_index`. Operators sizing container
/// memory should budget accordingly. A streaming/sax-style parse
/// would lower the peak to ~bytes-per-entry, but
/// `serde_json::StreamDeserializer` doesn't support `Vec<entry>`
/// shape directly without extra wrapper code; deferred to followup
/// `lsp-cpp-cdb-streaming-impl` if peak RSS becomes a
/// genuine container-memory constraint.
pub(crate) fn read_compile_commands(path: &Path) -> Result<Vec<String>> {
    let bytes = std::fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let arr = value.as_array().ok_or_else(|| {
        ShimError::Protocol(format!(
            "compile_commands at {} is not a JSON array",
            path.display()
        ))
    })?;
    let mut seen = std::collections::HashSet::with_capacity(arr.len());
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        if let Some(file) = entry.get("file").and_then(Value::as_str) {
            if seen.insert(file.to_string()) {
                out.push(file.to_string());
            }
        }
    }
    Ok(out)
}

/// Count `*.idx` shards in `dir`. Returns 0 if the directory does not
/// exist or cannot be read — caller treats that as "no shards yet".
pub(crate) fn count_shards(dir: &Path) -> usize {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    read.filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "idx")
                .unwrap_or(false)
        })
        .count()
}

/// Sum byte sizes of every `*.idx` shard in `dir`. Returns 0 on
/// missing dir, mirroring [`count_shards`].
pub(crate) fn sum_shard_bytes(dir: &Path) -> u64 {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    read.filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "idx")
                .unwrap_or(false)
        })
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Append a single timestamped line to the shim log so a caller
/// `tail -f`-ing `/tmp/lsp-cpp.log` (default) can watch the
/// build progress without parsing the eventual JSON response.
pub(crate) fn log_line(path: &Path, msg: &str) {
    let line = format!("{} {}\n", chrono_now_or_unix(), msg.trim_end_matches('\n'));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Best-effort wall-clock stamp for the log. We avoid pulling in
/// `chrono` (not currently a dep) and use the unix epoch seconds
/// instead — coarse but monotonic enough to cross-reference with
/// other log streams that timestamp the same way.
fn chrono_now_or_unix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("[t={}.{:03}]", d.as_secs(), d.subsec_millis()),
        Err(_) => "[t=?]".into(),
    }
}

impl LspBackend for Clangd {
    fn spawn(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let cc_dir = Self::resolve_compile_commands_dir(&self.project_root)?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let stderr = log_file.try_clone()?;

        // Validate index file presence up front so the caller gets a
        // structured `NoIndexFile` instead of a clangd-side load error.
        if let IndexMode::Full { index_file } | IndexMode::Hybrid { index_file } = &self.index_mode
        {
            if !index_file.exists() {
                return Err(ShimError::NoIndexFile {
                    path: index_file.display().to_string(),
                });
            }
        }

        let mut cmd = Command::new(&self.clangd_bin);
        // `--pch-storage=disk` MUST stay in lockstep with
        // `the upstream `clangd-launcher/clangd-driver.py` script` (line ~72) and the
        // `the upstream `clangd-launcher/start-clangd.sh` script` wrapper that launches it. clangd
        // keys on-disk index shards by a build-args fingerprint that
        // includes `--pch-storage`; if the shim launches with a different
        // value than the out-of-band indexer used to write the shards,
        // clangd treats the existing 730 MB / 66555-shard slab under
        // `.cache/clangd/index/` as alien and refuses to load it. Result:
        // every `workspace_symbol` call returns `[]` in 0 ms because the
        // in-memory symbol table is empty even though the disk shards are
        // present and complete. Predecessor branch
        // `lsp-cpp-bgindex-flag` (Outcome B) confirmed via live
        // PIDs 544087 (shim clangd) and 350653 (driver clangd) that this
        // flag was the only argv divergence.
        cmd.arg("--header-insertion=never")
            .arg("--clang-tidy=false")
            .arg("--pch-storage=disk")
            .arg(format!("-j={}", self.jobs))
            .arg(format!("--compile-commands-dir={}", cc_dir.display()));
        match &self.index_mode {
            IndexMode::Narrow => {
                cmd.arg("--background-index")
                    .arg("--background-index-priority=background");
            }
            IndexMode::Full { index_file } => {
                // Read-only against the pre-built index; no live
                // background indexer.
                cmd.arg(format!("--index-file={}", index_file.display()));
            }
            IndexMode::Hybrid { index_file } => {
                // Pre-built seed plus live background indexer for
                // gaps. clangd 19 honours `--index-file` and
                // `--background-index` together.
                cmd.arg(format!("--index-file={}", index_file.display()))
                    .arg("--background-index")
                    .arg("--background-index-priority=background");
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ShimError::ClangdMissing {
                path: self.clangd_bin.display().to_string(),
            },
            _ => ShimError::Io(e),
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ShimError::Protocol("clangd stdin handle missing".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| ShimError::Protocol("clangd stdout handle missing".into()))?,
        );
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(stdout);

        let init_id = self.next_request_id();
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{}", self.project_root.display()),
            "capabilities": {
                "workspace": { "symbol": { "dynamicRegistration": false } },
                "textDocument": {
                    "definition": {},
                    "references": {},
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("just set");
        jsonrpc::send(
            stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": init_id,
                "method": "initialize",
                "params": init_params,
            }),
        )?;
        let stdout = self.stdout.as_mut().expect("just set");
        let deadline = Instant::now() + Duration::from_secs(self.initialize_timeout_s);
        loop {
            if Instant::now() > deadline {
                return Err(ShimError::InitializeTimeout {
                    timeout_s: self.initialize_timeout_s,
                    log_path: self.log_path.display().to_string(),
                });
            }
            let value = jsonrpc::recv(&mut *stdout)?;
            if value.get("id").and_then(|v| v.as_i64()) == Some(init_id) {
                if let Some(err) = value.get("error") {
                    return Err(ShimError::Protocol(format!(
                        "clangd initialize error: {err}"
                    )));
                }
                break;
            }
        }
        // initialized notification — clangd starts background indexing
        // after this.
        self.notify("initialized", json!({}))?;
        // Optionally seed clangd's dynamic index with the headers
        // listed in `LSP_CPP_SEED_HEADERS` so workspace_symbol returns
        // those types on the first query rather than waiting for
        // `--background-index` shards (which only build under
        // IndexMode::Narrow / Hybrid and even then take seconds to
        // land on disk). Default empty — when unset this is a no-op.
        // Best-effort per file — never fails spawn.
        self.seed_didopen();
        Ok(())
    }

    fn definition(&mut self, path: &str, line: u32, column: u32) -> Result<Vec<Location>> {
        let uri = format!("file://{path}");
        let result = self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line.saturating_sub(1), "character": column.saturating_sub(1) },
            }),
        )?;
        Ok(parse_locations(&result))
    }

    fn references(&mut self, path: &str, line: u32, column: u32) -> Result<Vec<Location>> {
        let uri = format!("file://{path}");
        let result = self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line.saturating_sub(1), "character": column.saturating_sub(1) },
                "context": { "includeDeclaration": true },
            }),
        )?;
        Ok(parse_locations(&result))
    }

    fn hover(&mut self, path: &str, line: u32, column: u32) -> Result<Option<Hover>> {
        let uri = format!("file://{path}");
        let result = self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line.saturating_sub(1), "character": column.saturating_sub(1) },
            }),
        )?;
        if result.is_null() {
            return Ok(None);
        }
        let contents = result
            .get("contents")
            .map(|c| match c {
                Value::String(s) => s.clone(),
                Value::Object(map) => map
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|i| i.as_str().or_else(|| i.get("value")?.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            })
            .unwrap_or_default();
        if contents.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Hover { contents }))
        }
    }

    fn workspace_symbol(&mut self, query: &str) -> Result<Vec<Symbol>> {
        let result = self.request("workspace/symbol", json!({ "query": query }))?;
        let arr = result.as_array().cloned().unwrap_or_default();
        Ok(arr
            .into_iter()
            .filter_map(|v| {
                let name = v.get("name")?.as_str()?.to_string();
                let kind = v.get("kind")?.as_u64()? as u32;
                let loc = v.get("location")?;
                let location = parse_one_location(loc)?;
                let container = v
                    .get("containerName")
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string());
                Some(Symbol {
                    name,
                    kind,
                    location,
                    container,
                })
            })
            .collect())
    }

    fn shutdown(&mut self) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        // Best-effort. If clangd is wedged we still want to kill it.
        let _ = self.request("shutdown", Value::Null);
        let _ = self.notify("exit", Value::Null);
        if let Some(stdin) = self.stdin.take() {
            drop(stdin);
        }
        if let Some(mut child) = self.child.take() {
            // Give clangd 2 s to flush index, then SIGKILL.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait()? {
                    Some(_) => break,
                    None if Instant::now() > deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    None => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        }
        Ok(())
    }
}

impl Drop for Clangd {
    fn drop(&mut self) {
        // Best-effort, errors swallowed.
        let _ = self.shutdown();
    }
}

/// Append one line to the shim log. Best-effort — failures are
/// swallowed because seed-didOpen logging is not load-bearing for
/// correctness; if the log file cannot be opened (read-only FS, full
/// disk) the seed step still ran successfully on the wire.
fn log_seed_event(log_path: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn parse_locations(value: &Value) -> Vec<Location> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().filter_map(parse_one_location).collect(),
        Value::Object(_) => parse_one_location(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn parse_one_location(value: &Value) -> Option<Location> {
    let uri = value.get("uri")?.as_str()?;
    let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    let range = value.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32 + 1;
    let column = start.get("character")?.as_u64()? as u32 + 1;
    Some(Location { path, line, column })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// Serialises tests that mutate or observe process-wide environment
    /// variables. `cargo test` runs the suite in parallel by default; without
    /// this lock, one test's `set_var`/`remove_var` races a sister test's
    /// `IndexMode::from_env()` read, producing wrong-mode returns and
    /// intermittent CI flakes.
    ///
    /// Held for the duration of the env-touching test; poisoning is recovered
    /// (a panic with the lock held still leaves the env in whatever state the
    /// panicking test set, so subsequent tests reset to a known state at the
    /// top of their critical section).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: acquire `ENV_LOCK`, recovering from poison.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn resolve_prefers_narrow_when_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.narrow.json"), b"[]").unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let resolved = Clangd::resolve_compile_commands_dir(dir.path()).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_falls_back_to_full() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let resolved = Clangd::resolve_compile_commands_dir(dir.path()).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_errors_when_neither_present() {
        let dir = tempfile::tempdir().unwrap();
        let err = Clangd::resolve_compile_commands_dir(dir.path()).unwrap_err();
        assert!(
            matches!(err, ShimError::NoCompileCommands { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_locations_handles_array_object_and_null() {
        let arr = json!([{
            "uri": "file:///tmp/foo.cpp",
            "range": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 7}}
        }]);
        let locs = parse_locations(&arr);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, "/tmp/foo.cpp");
        assert_eq!(locs[0].line, 10);
        assert_eq!(locs[0].column, 5);

        let single = json!({
            "uri": "file:///tmp/bar.cpp",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        });
        let locs = parse_locations(&single);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, "/tmp/bar.cpp");

        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn missing_clangd_binary_reports_structured_error() {
        let _env_guard = lock_env();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        std::env::set_var("CLANGD_BIN", "/definitely/not/a/real/path/clangd-xyz");
        let mut backend = Clangd::new(dir.path());
        let err = backend.spawn().unwrap_err();
        std::env::remove_var("CLANGD_BIN");
        assert!(
            matches!(err, ShimError::ClangdMissing { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn missing_compile_commands_reports_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = Clangd::new(dir.path());
        let err = backend.spawn().unwrap_err();
        assert!(
            matches!(err, ShimError::NoCompileCommands { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn full_mode_with_missing_index_reports_structured_error() {
        // Regression guard: switching to Full / Hybrid mode without
        // a pre-built index file present must surface as
        // `NoIndexFile`, NOT a clangd-side load error or a silent
        // fall-through to live indexing.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let bogus = dir.path().join("does-not-exist.idx");
        let mut backend = Clangd::new(dir.path()).with_index_mode(IndexMode::Full {
            index_file: bogus.clone(),
        });
        let err = backend.spawn().unwrap_err();
        match err {
            ShimError::NoIndexFile { ref path } => assert_eq!(path, &bogus.display().to_string()),
            other => panic!("expected NoIndexFile, got {other:?}"),
        }
    }

    #[test]
    fn hybrid_mode_with_missing_index_reports_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let bogus = dir.path().join("missing.idx");
        let mut backend =
            Clangd::new(dir.path()).with_index_mode(IndexMode::Hybrid { index_file: bogus });
        let err = backend.spawn().unwrap_err();
        assert!(matches!(err, ShimError::NoIndexFile { .. }), "got {err:?}");
    }

    #[test]
    fn index_mode_from_env_defaults_to_narrow() {
        let _env_guard = lock_env();
        // Make sure no leftover env var from a previous test trips us.
        std::env::remove_var("LSP_CPP_INDEX_MODE");
        std::env::remove_var("LSP_CPP_INDEX_FILE");
        assert_eq!(IndexMode::from_env(), IndexMode::Narrow);
    }

    #[test]
    fn index_mode_from_env_parses_full_with_explicit_path() {
        let _env_guard = lock_env();
        std::env::set_var("LSP_CPP_INDEX_MODE", "full");
        std::env::set_var("LSP_CPP_INDEX_FILE", "/tmp/probe.idx");
        let mode = IndexMode::from_env();
        std::env::remove_var("LSP_CPP_INDEX_MODE");
        std::env::remove_var("LSP_CPP_INDEX_FILE");
        match mode {
            IndexMode::Full { index_file } => {
                assert_eq!(index_file, PathBuf::from("/tmp/probe.idx"));
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn index_mode_from_env_concurrent_set_and_read_serialised() {
        // Regression guard for LOW-SQ2: each worker thread independently
        // acquires `ENV_LOCK`, sets `LSP_CPP_INDEX_MODE` + `_INDEX_FILE`
        // to its own values, calls `IndexMode::from_env()`, asserts the read
        // matches what it set, then restores the env. With the lock in
        // place this is sequential; without it, thread B's `remove_var`
        // can land between thread A's `set_var` and A's `from_env()`,
        // yielding `Narrow` where `Full` was asserted (or arbitrary file
        // path crosstalk between A and B).
        //
        // Mutation probe: replace `lock_env()` with `Mutex::new(()).lock().unwrap()`
        // (a per-call fresh mutex, no shared exclusion) and run
        // `cargo test -p lsp-cpp --lib index_mode_from_env_concurrent
        //   -- --test-threads=8` in a loop — the assertion will fire because
        // workers no longer serialise on a shared lock.

        let mut handles = Vec::new();
        for i in 0..8u32 {
            handles.push(std::thread::spawn(move || {
                let _g = lock_env();
                let want_path = format!("/tmp/probe-{i}.idx");
                std::env::set_var("LSP_CPP_INDEX_MODE", "full");
                std::env::set_var("LSP_CPP_INDEX_FILE", &want_path);
                let got = IndexMode::from_env();
                std::env::remove_var("LSP_CPP_INDEX_MODE");
                std::env::remove_var("LSP_CPP_INDEX_FILE");
                match got {
                    IndexMode::Full { index_file } => {
                        assert_eq!(
                            index_file,
                            PathBuf::from(&want_path),
                            "thread {i}: env-state crosstalk — set {want_path}, read {}",
                            index_file.display()
                        );
                    }
                    other => panic!("thread {i}: expected Full, got {other:?}"),
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn pick_compile_commands_prefers_full_over_narrow() {
        // The full DB takes precedence because the tool's name —
        // build_full_index — is about expanding coverage beyond the
        // narrow subset. Deleting the preference flips the assertion.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        fs::write(dir.path().join("compile_commands.narrow.json"), b"[]").unwrap();
        let picked = pick_compile_commands_path(dir.path()).unwrap();
        assert_eq!(picked, dir.path().join("compile_commands.json"));
    }

    #[test]
    fn pick_compile_commands_falls_back_to_narrow() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.narrow.json"), b"[]").unwrap();
        let picked = pick_compile_commands_path(dir.path()).unwrap();
        assert_eq!(picked, dir.path().join("compile_commands.narrow.json"));
    }

    #[test]
    fn pick_compile_commands_errors_when_neither_present() {
        let dir = tempfile::tempdir().unwrap();
        let err = pick_compile_commands_path(dir.path()).unwrap_err();
        assert!(matches!(err, ShimError::NoCompileCommands { .. }));
    }

    #[test]
    fn read_compile_commands_dedupes_by_file_in_order() {
        // Two entries for the same TU (different build configs) should
        // collapse to one, with first occurrence order preserved.
        let dir = tempfile::tempdir().unwrap();
        let cdb = dir.path().join("compile_commands.json");
        fs::write(
            &cdb,
            br#"[
                {"directory":"/x","file":"/a.cpp","command":"clang++ /a.cpp"},
                {"directory":"/x","file":"/b.cpp","command":"clang++ /b.cpp"},
                {"directory":"/x","file":"/a.cpp","command":"clang++ /a.cpp -DFOO"}
            ]"#,
        )
        .unwrap();
        let files = read_compile_commands(&cdb).unwrap();
        assert_eq!(files, vec!["/a.cpp".to_string(), "/b.cpp".to_string()]);
    }

    #[test]
    fn read_compile_commands_rejects_non_array_root() {
        let dir = tempfile::tempdir().unwrap();
        let cdb = dir.path().join("compile_commands.json");
        fs::write(&cdb, br#"{"file":"/a.cpp"}"#).unwrap();
        let err = read_compile_commands(&cdb).unwrap_err();
        assert!(matches!(err, ShimError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn count_shards_handles_missing_dir_and_filters_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        assert_eq!(count_shards(&nonexistent), 0);
        // Mixed dir: 2 .idx, 1 .txt → count == 2.
        let mixed = dir.path().join("idx_dir");
        fs::create_dir(&mixed).unwrap();
        fs::write(mixed.join("a.idx"), b"x").unwrap();
        fs::write(mixed.join("b.idx"), b"yy").unwrap();
        fs::write(mixed.join("c.txt"), b"zzz").unwrap();
        assert_eq!(count_shards(&mixed), 2);
        assert_eq!(sum_shard_bytes(&mixed), 3); // 1 + 2 bytes; .txt excluded
    }

    #[test]
    fn build_full_index_rejects_full_index_mode() {
        // Full mode disables --background-index, so the tool can't do
        // its job. Surface as Protocol error rather than silently
        // succeeding with zero new shards.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let bogus = dir.path().join("idx");
        fs::write(&bogus, b"").unwrap();
        let mut backend =
            Clangd::new(dir.path()).with_index_mode(IndexMode::Full { index_file: bogus });
        let err = backend.build_full_index(50).unwrap_err();
        match err {
            ShimError::Protocol(msg) => assert!(
                msg.contains("IndexMode::Full"),
                "expected mode-rejection message, got {msg}"
            ),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// End-to-end test against the host's clangd-19 + the real C++ project
    /// compile_commands.json. `#[ignore]`d because it requires:
    ///   - clangd-19 on PATH (or `CLANGD_BIN`),
    ///   - `/path/to/project/compile_commands.json` to
    ///     exist,
    ///   - write access to
    ///     `/path/to/project/.cache/clangd/index/`,
    ///   - 30-90 s of wall time for a `max_tus=50` run (depends on
    ///     PCH cache state).
    ///
    /// Run with:
    ///     cargo test -p lsp-cpp -- --ignored \
    ///         build_full_index_smoke
    ///
    /// Asserts:
    ///   - The report comes back with `tus_opened` between 45 and 50
    ///     (a few entries can be skipped for I/O errors on
    ///     UHT-generated transient TUs).
    ///   - `shards_on_disk_after >= shards_before + 1` (the run wrote
    ///     at least one new shard; the precise increment depends on
    ///     which TUs were already cached). We deliberately do NOT
    ///     require `+ 50` because every cached TU's shard already
    ///     exists from the pre-existing `.cache/clangd/index/`
    ///     contents, and the run mostly re-validates rather than
    ///     re-writing them.
    #[test]
    #[ignore]
    fn build_full_index_smoke() {
        let project = std::path::PathBuf::from("/path/to/project");
        if !project.join("compile_commands.json").exists() {
            eprintln!(
                "skipping build_full_index_smoke: compile_commands.json missing under {}",
                project.display()
            );
            return;
        }
        let index_dir = project.join(".cache/clangd/index");
        let shards_before = count_shards(&index_dir);
        let mut backend = Clangd::new(&project);
        let report = backend
            .build_full_index(50)
            .expect("build_full_index should succeed against the host project checkout");
        eprintln!(
            "build_full_index_smoke report: {}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        assert!(
            report.tus_opened >= 45 && report.tus_opened <= 50,
            "expected tus_opened in 45..=50, got {}",
            report.tus_opened
        );
        assert!(
            report.shards_on_disk_after >= shards_before + 1,
            "expected at least one new shard; shards_before={shards_before}, after={}",
            report.shards_on_disk_after
        );
        assert!(
            report.wall_seconds > 0.0,
            "wall_seconds should be positive, got {}",
            report.wall_seconds
        );
        assert!(
            report.shards_size_bytes > 0,
            "shards_size_bytes should be positive after a run"
        );
    }

    /// Per-method timeout selector: workspace/symbol gets a 90 s floor
    /// (v0.3 Patch A bumped from 30 s) because cold UE-engine queries
    /// against a project that hasn't yet finished its seed-didOpen TU
    /// closure parses can run 60 s+ before the in-memory symbol table
    /// is populated; position queries inherit the configured 60 s.
    /// Deleting the `match` arm in `request_timeout_for_method` makes
    /// this test fail.
    #[test]
    fn workspace_symbol_uses_longer_timeout_than_position_queries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        let backend = Clangd::new(dir.path());
        // Default request_timeout_s is 60.
        assert_eq!(
            backend.request_timeout_for_method("textDocument/definition"),
            60
        );
        assert_eq!(
            backend.request_timeout_for_method("textDocument/references"),
            60
        );
        assert_eq!(backend.request_timeout_for_method("textDocument/hover"), 60);
        assert_eq!(
            backend.request_timeout_for_method("workspace/symbol"),
            90,
            "v0.3 Patch A: workspace/symbol uses 90s floor"
        );
    }

    /// `classify_timeout` against an alive subprocess returns
    /// `ClangdBusy` (NOT `ClangdExited`, NOT a generic `RequestTimeout`,
    /// NOT a broken-pipe surface). This is the busy-vs-broken taxonomy
    /// boundary the dispatch prompt requires: the prior code path
    /// returned bare `RequestTimeout` for both alive-but-slow and
    /// exited-and-broken; this test pins the new alive→busy mapping.
    ///
    /// Counterfactual: replace `child.try_wait()` with `Ok(Some(0))`
    /// in classify_timeout and this test will fail because the helper
    /// will start returning `ClangdExited` for an alive subprocess.
    #[test]
    fn classify_timeout_alive_subprocess_returns_busy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        // Use `sleep 30` as a stand-in for a busy clangd. Spawned
        // directly with std::process::Command so we don't need to bring
        // up a real clangd.
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep should be on PATH");
        let mut backend = Clangd::new(dir.path());
        backend.child = Some(child);
        let err = backend.classify_timeout("textDocument/definition", 1);
        // Kill the sleep before asserting so a flaky test doesn't
        // leak a 30 s subprocess.
        if let Some(mut c) = backend.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        match err {
            ShimError::ClangdBusy {
                method, timeout_s, ..
            } => {
                assert_eq!(method, "textDocument/definition");
                assert_eq!(timeout_s, 1);
            }
            other => panic!("expected ClangdBusy for alive subprocess, got {other:?}"),
        }
    }

    /// Counterfactual cover for the alive-vs-dead branch in
    /// `classify_timeout`: an exited subprocess (sleep already reaped)
    /// surfaces as `ClangdExited`, which the supervisor (a follow-up branch
    /// `lsp-cpp-auto-resume-wrapper`) consumes as the
    /// restart-trigger signal — distinct from `ClangdBusy` which is a
    /// retry-with-longer-timeout signal.
    #[test]
    fn classify_timeout_exited_subprocess_returns_exited() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compile_commands.json"), b"[]").unwrap();
        // Spawn `true` (exits immediately) and wait for it to finish
        // BEFORE calling classify_timeout, so try_wait returns
        // Ok(Some(_)).
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("true should be on PATH");
        // Give it a moment to exit, then reap-detect via try_wait
        // until Some.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() > deadline => panic!("true did not exit in 2s"),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
        let mut backend = Clangd::new(dir.path());
        backend.child = Some(child);
        let err = backend.classify_timeout("workspace/symbol", 1);
        match err {
            ShimError::ClangdExited { status, .. } => {
                assert_eq!(status, "exited_during_request");
            }
            other => panic!("expected ClangdExited for exited subprocess, got {other:?}"),
        }
    }
}
