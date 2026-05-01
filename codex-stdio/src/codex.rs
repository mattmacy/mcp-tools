//! Codex backend client — shells out to the existing `codex` CLI.
//! The CLI handles auth via ChatGPT OAuth (`~/.codex/auth.json`)
//! so we do NOT need `OPENAI_API_KEY` plumbing.
//!
//! Why shell out instead of registering native `codex mcp-server`
//! directly: this shim's stable contract is `codex_health` +
//! `codex_run_task`. Native `codex` MCP exposes different tool
//! names (`codex` + `codex-reply`) with a different schema. The
//! shim provides a stable surface while delegating actual work to
//! whatever Codex install is current. When upstream ships breaking
//! changes on the native MCP surface, the shim is re-pinned once
//! instead of churning every caller.
//!
//! The [`Client`] trait abstracts the transport so tests can
//! substitute [`ReplayClient`] (reads a recorded JSON body from
//! disk). The MCP server picks the impl at request time based on
//! `CODEX_STDIO_REPLAY_FIXTURE`. Production path uses
//! [`CodexExecClient`] which spawns
//! `codex exec --cd <wt> --sandbox workspace-write --json` and
//! parses the JSONL event stream for the agent's final message.

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// One Chat Completions request. We expose only the subset we use.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChatRequest {
    /// Model name, e.g. `gpt-5.3-codex`.
    pub(crate) model: String,
    /// Conversation messages; we send a single-turn `user` message
    /// holding the task packet.
    pub(crate) messages: Vec<ChatMessage>,
    /// Optional output ceiling. Maps to OpenAI's `max_completion_tokens`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "max_completion_tokens"
    )]
    pub(crate) max_tokens: Option<u64>,
}

/// One message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    /// `system` / `user` / `assistant`. Stays a free-form string so
    /// future role additions on OpenAI's side don't require a code
    /// change here.
    pub(crate) role: String,
    /// Message text body.
    pub(crate) content: String,
}

/// Subset of the OpenAI Chat Completions response we care about.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChatResponse {
    /// Server-assigned response id (forwarded to the caller for
    /// audit / billing reconciliation).
    #[serde(default)]
    pub(crate) id: String,
    /// Server-reported model name, may differ from the requested
    /// model when OpenAI routes to a fallback.
    #[serde(default)]
    pub(crate) model: String,
    /// One or more completion choices. The shim only consumes the
    /// first `[0].message.content`.
    pub(crate) choices: Vec<Choice>,
    /// Token-usage accounting. Optional because some error paths
    /// omit it.
    pub(crate) usage: Option<Usage>,
}

/// One completion choice.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Choice {
    /// The assistant's message in this choice slot.
    pub(crate) message: ChatMessage,
    /// Why the model stopped emitting tokens (`stop`, `length`, …).
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

/// Token counts billed to the API key.
///
/// `prompt_tokens` / `completion_tokens` / `total_tokens` follow the OpenAI
/// Chat Completions shape. Two extra optional fields are populated only
/// from the Codex `--json` event stream (`turn.completed.usage`) and stay
/// `None` on the OpenAI HTTP path; they are NOT sent back over the wire to
/// the routing skill in the standard `tokens_used` block — they surface in
/// `tokens_used_extended` in [`run_task::dispatch`] instead so Anthropic-
/// path callers get a stable shape.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct Usage {
    /// Input + system + tool-schema tokens.
    #[serde(default)]
    pub(crate) prompt_tokens: u64,
    /// Output tokens (the diff body, primarily).
    #[serde(default)]
    pub(crate) completion_tokens: u64,
    /// Sum of the two; OpenAI sends this redundantly.
    #[serde(default)]
    pub(crate) total_tokens: u64,
    /// Cached-prompt-token count from Codex's prompt cache. Populated
    /// from `turn.completed.usage.cached_input_tokens`. `None` on the
    /// OpenAI HTTP path because the standard `usage` block does not
    /// expose a cache-hit axis.
    #[serde(default)]
    pub(crate) cached_prompt_tokens: Option<u64>,
    /// Reasoning-output tokens spent (separate from completion tokens).
    /// Populated from `turn.completed.usage.reasoning_output_tokens`.
    /// `None` on the OpenAI HTTP path; non-`None` for Codex models that
    /// run a reasoning pass.
    #[serde(default)]
    pub(crate) reasoning_output_tokens: Option<u64>,
}

/// Transport surface — the MCP server picks one impl per request.
pub(crate) trait Client {
    /// Send a Chat Completions request and parse the response.
    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, String>;
}

/// Production client — spawns `codex exec` with the prompt on stdin.
/// Auth is handled by the `codex` CLI itself (ChatGPT OAuth or API
/// key, whichever was configured via `codex login`). We pass `--cd`
/// for the worktree boundary, `--sandbox workspace-write` so file
/// writes are confined, and `--json` so we get a stable JSONL event
/// stream instead of having to scrape the human-readable transcript.
pub(crate) struct CodexExecClient {
    /// Path to the `codex` binary. Typically `/usr/bin/codex` on
    /// container installs of npm package `@openai/codex`; env
    /// override `CODEX_STDIO_BIN` for testability + alternate
    /// installs.
    pub(crate) binary_path: PathBuf,
    /// Worktree the worker is constrained to — passed to
    /// `codex exec --cd <path>`. Already canonicalized + boundary-
    /// checked by the caller.
    pub(crate) worktree_path: PathBuf,
}

impl CodexExecClient {
    /// Build a client pointed at an explicit binary + worktree.
    pub(crate) fn new(binary_path: PathBuf, worktree_path: PathBuf) -> Self {
        Self {
            binary_path,
            worktree_path,
        }
    }
}

/// Read `CODEX_STDIO_SANDBOX_BYPASS` and decide whether to swap the default
/// `--sandbox workspace-write` pair for `--dangerously-bypass-approvals-and-sandbox`.
///
/// Truthy values: `1`, `true` (case-insensitive). Anything else (unset, empty,
/// `0`, `false`, garbage) returns `false` and keeps the safe default.
///
/// # Why this gate exists
///
/// The default `--sandbox workspace-write` mode invokes `bwrap` to drop the
/// child process into a user namespace with a confined view of the
/// filesystem. In some container environments that fails with
/// `bwrap: No permissions to create a new namespace` because docker-default
/// seccomp blocks the `unshare(CLONE_NEWUSER)` syscall AND newer Ubuntu
/// releases set `kernel.apparmor_restrict_unprivileged_userns=1`. With the
/// sandbox broken, every `codex exec` call exits non-zero before the model
/// is even reached.
///
/// `--dangerously-bypass-approvals-and-sandbox` is the codex CLI's official
/// "I know what I'm doing" flag — it skips the sandbox entirely and runs
/// the model's tool calls directly against the host.
///
/// # Security regression — opt-in only
///
/// With the bypass enabled, the model can invoke `apply_patch` outside the
/// worktree boundary that [`crate::run_task::dispatch`] enforces via
/// `validate_worktree_path`. Acceptable only in trusted dev environments
/// where the harness already runs without permission prompts. NOT
/// acceptable in production. The default stays OFF; flip via env var.
pub(crate) fn sandbox_bypass_from_env() -> bool {
    match std::env::var("CODEX_STDIO_SANDBOX_BYPASS") {
        Ok(v) => {
            let lc = v.trim().to_ascii_lowercase();
            lc == "1" || lc == "true"
        }
        Err(_) => false,
    }
}

/// Build the `codex exec ...` argv that [`CodexExecClient::chat`] feeds to
/// [`Command`]. Exposed as a free fn (not a method) so unit tests can
/// inspect the resulting argv without spawning a subprocess.
///
/// `bypass_sandbox=false` (the default, also used in production environments
/// where bwrap works) emits `--sandbox workspace-write`. `bypass_sandbox=true`
/// emits `--dangerously-bypass-approvals-and-sandbox` and OMITS the `--sandbox`
/// pair entirely — the codex CLI rejects passing both at once.
pub(crate) fn build_codex_exec_args(
    worktree_path: &std::path::Path,
    out_path: &std::path::Path,
    model: &str,
    max_tokens: Option<u64>,
    bypass_sandbox: bool,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut args: Vec<OsString> = Vec::with_capacity(16);
    args.push(OsString::from("exec"));
    args.push(OsString::from("--cd"));
    args.push(worktree_path.as_os_str().to_owned());
    if bypass_sandbox {
        args.push(OsString::from("--dangerously-bypass-approvals-and-sandbox"));
    } else {
        args.push(OsString::from("--sandbox"));
        args.push(OsString::from("workspace-write"));
    }
    args.push(OsString::from("--skip-git-repo-check"));
    args.push(OsString::from("--output-last-message"));
    args.push(out_path.as_os_str().to_owned());
    args.push(OsString::from("--color"));
    args.push(OsString::from("never"));
    // `--json` makes stdout a JSONL event stream. The `thread.started`
    // event carries the real thread id and the `turn.completed` event
    // carries `usage` (input, cached_input, output, reasoning_output).
    // Without this flag the shim has to fabricate an id from PID and
    // ship `usage = None`.
    args.push(OsString::from("--json"));
    args.push(OsString::from("-m"));
    args.push(OsString::from(model));
    if let Some(max) = max_tokens {
        // Codex exposes max_output_tokens via -c; defer the exact key
        // name to operator override since the CLI surface is not yet
        // locked.
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("model_max_output_tokens={max}")));
    }
    // Prompt comes via stdin so it does not appear in `ps`.
    args.push(OsString::from("-"));
    args
}

impl Client for CodexExecClient {
    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, String> {
        // Concatenate the system + user messages into a single
        // prompt body. `codex exec` does not have a separate
        // system-message channel; its `developer-instructions`
        // analogue is the `--config base_instructions=...` knob,
        // which we elide for now since our system prompt is already
        // small (the worktree-cwd note + Outcome A/B contract).
        let mut prompt = String::new();
        for m in &req.messages {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            if m.role != "user" {
                prompt.push_str(&format!("[{}]\n", m.role));
            }
            prompt.push_str(&m.content);
        }

        // Stream the result to a temp file via -o so we can read
        // the agent's final message back without scraping JSONL.
        let outdir = std::env::temp_dir();
        let out_path = outdir.join(format!("codex-stdio-out-{}.txt", std::process::id()));

        let args = build_codex_exec_args(
            &self.worktree_path,
            &out_path,
            &req.model,
            req.max_tokens,
            sandbox_bypass_from_env(),
        );
        let mut cmd = Command::new(&self.binary_path);
        cmd.args(&args);
        // Prompt comes via stdin so it does not appear in `ps`.
        cmd.stdin(std::process::Stdio::piped());

        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn codex exec: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write as _;
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|e| format!("write codex stdin: {e}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("wait codex exec: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&out_path);
            return Err(format!(
                "codex exec failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim(),
            ));
        }

        let final_message =
            std::fs::read_to_string(&out_path).map_err(|e| format!("read codex output: {e}"))?;
        let _ = std::fs::remove_file(&out_path);

        // Parse the `--json` event stream from stdout. A missing or
        // malformed stream is non-fatal: the diff body still came
        // through the `--output-last-message` file, so we degrade
        // gracefully (id falls back to PID-based, usage to None) and
        // let the caller proceed.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_codex_jsonl(&stdout);
        let id = parsed
            .thread_id
            .unwrap_or_else(|| format!("codex-exec-{}", std::process::id()));

        Ok(ChatResponse {
            id,
            model: req.model.clone(),
            choices: vec![Choice {
                message: ChatMessage {
                    role: "assistant".into(),
                    content: final_message,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: parsed.usage,
        })
    }
}

/// Result of walking the `codex exec --json` event stream. Both fields
/// are `Option` because the stream may be truncated (transport error
/// after the diff was written) or noisy (warnings interleaved). The
/// caller falls back to PID-based id and `usage = None` when either is
/// missing.
#[derive(Debug, Default)]
pub(crate) struct ParsedCodexEvents {
    /// Real Codex `thread_id` from the `thread.started` event. Used as
    /// `ChatResponse.id` for billing reconciliation against
    /// `~/.codex/sessions/`.
    pub(crate) thread_id: Option<String>,
    /// Token usage from the `turn.completed.usage` block.
    pub(crate) usage: Option<Usage>,
}

/// Walk `codex exec --json` JSONL stdout. Looks for two event types:
///
///   - `{"type":"thread.started","thread_id":"<id>"}` — emitted once
///     per session at the start of the stream.
///   - `{"type":"turn.completed","usage":{...}}` — emitted once per
///     completed turn at the end. We take the LAST one we see (in case
///     the worker did multi-turn work; only the final tally is what
///     gets billed).
///
/// Non-JSON lines (CLI warnings, error backtraces) are skipped silently
/// — the agent's diff body comes through `--output-last-message`, not
/// stdout, so noise on stdout cannot corrupt the result.
pub(crate) fn parse_codex_jsonl(stdout: &str) -> ParsedCodexEvents {
    let mut out = ParsedCodexEvents::default();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "thread.started" => {
                if let Some(id) = v.get("thread_id").and_then(|t| t.as_str()) {
                    out.thread_id = Some(id.to_string());
                }
            }
            "turn.completed" => {
                if let Some(u) = v.get("usage") {
                    let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    let cached = u.get("cached_input_tokens").and_then(|x| x.as_u64());
                    let output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    let reasoning = u.get("reasoning_output_tokens").and_then(|x| x.as_u64());
                    out.usage = Some(Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        // Codex `turn.completed.usage` does not surface a
                        // canonical `total_tokens` field in the JSONL schema
                        // we consume here; the event exposes input/output plus
                        // Codex-only side-band axes for cache hits and
                        // reasoning. We intentionally mirror the OpenAI Chat
                        // Completions convention in `tokens_used.total` by
                        // summing only prompt + completion, while preserving
                        // cached/reasoning separately in
                        // `tokens_used_extended`.
                        total_tokens: input + output,
                        cached_prompt_tokens: cached,
                        reasoning_output_tokens: reasoning,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Replay client — reads a recorded Chat Completions response from
/// disk. Lets `cargo test` exercise the full MCP wire shape without
/// `OPENAI_API_KEY` or network access.
///
/// The fixture file MUST be a valid Chat Completions response body.
/// Capture one in production with `curl -sS ... > fixture.json` and
/// commit the redacted form under `tests/fixtures/replay/`.
pub(crate) struct ReplayClient {
    /// Absolute path to the JSON fixture.
    pub(crate) fixture_path: PathBuf,
}

impl ReplayClient {
    /// Build a replay client against an explicit fixture path.
    pub(crate) fn new(fixture_path: PathBuf) -> Self {
        Self { fixture_path }
    }
}

impl Client for ReplayClient {
    fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, String> {
        // Suffix-dispatch: .jsonl → walk codex exec --json event stream
        // (exercises parse_codex_jsonl code path in test). Anything else
        // (.json or no extension) → legacy Chat Completions body parse.
        let is_jsonl = self
            .fixture_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("jsonl"))
            .unwrap_or(false);
        if is_jsonl {
            return self.replay_jsonl();
        }
        let body = std::fs::read_to_string(&self.fixture_path)
            .map_err(|e| format!("read replay fixture {}: {e}", self.fixture_path.display()))?;
        serde_json::from_str(&body)
            .map_err(|e| format!("parse replay fixture {}: {e}", self.fixture_path.display()))
    }
}

impl ReplayClient {
    /// Replay a recorded `codex exec --json` event stream from disk.
    ///
    /// Parses the JSONL via [`parse_codex_jsonl`] (same path production
    /// `CodexExecClient` uses), then synthesizes a `ChatResponse` whose
    /// `id` comes from `thread.started.thread_id`, whose
    /// `choices[0].message.content` comes from the LAST
    /// `item.completed.item.text` in the stream, and whose `usage` comes
    /// from `turn.completed.usage`.
    ///
    /// Missing `thread.started` falls back to a synthetic id so the
    /// fixture remains usable for partial-stream regression tests.
    fn replay_jsonl(&self) -> Result<ChatResponse, String> {
        let body = std::fs::read_to_string(&self.fixture_path)
            .map_err(|e| format!("read replay fixture {}: {e}", self.fixture_path.display()))?;
        let parsed = parse_codex_jsonl(&body);
        let agent_text = extract_last_agent_message(&body).unwrap_or_default();
        let id = parsed
            .thread_id
            .unwrap_or_else(|| "codex-replay-no-thread".to_string());
        Ok(ChatResponse {
            id,
            model: String::new(),
            choices: vec![Choice {
                message: ChatMessage {
                    role: "assistant".into(),
                    content: agent_text,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: parsed.usage,
        })
    }
}

/// Walk the JSONL event stream a second time to pull out the LAST
/// `item.completed` event whose `item.type == "agent_message"`. Kept
/// separate from [`parse_codex_jsonl`] so the production parser stays
/// focused on usage + thread-id (the only two things production cares
/// about — production reads the diff body via `--output-last-message`
/// not from stdout).
fn extract_last_agent_message(stdout: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("item.completed") {
            continue;
        }
        let item = match v.get("item") {
            Some(i) => i,
            None => continue,
        };
        if item.get("type").and_then(|t| t.as_str()) != Some("agent_message") {
            continue;
        }
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            last = Some(text.to_string());
        }
    }
    last
}

/// Build the appropriate transport for a given worktree.
///
/// Selection order:
///
/// 1. If `CODEX_STDIO_REPLAY_FIXTURE` is set, return a [`ReplayClient`]
///    (worktree path ignored — fixture path is the data source).
/// 2. Else, locate the `codex` binary (env `CODEX_STDIO_BIN`, then
///    `which codex`, then the canonical `/usr/bin/codex`). If the
///    binary is not on disk, return `Err` so the caller can surface
///    a structured `available: false` reason.
///
/// Auth is handled by the `codex` CLI itself via ChatGPT OAuth
/// (`~/.codex/auth.json`) or `OPENAI_API_KEY` if `codex login
/// --with-api-key` was used. The shim does NOT check creds — that
/// is the CLI's job, and surfaces as a non-zero exit on the first
/// `codex exec` call if missing.
pub(crate) fn for_worktree(worktree_path: &std::path::Path) -> Result<Box<dyn Client>, String> {
    if let Ok(fixture) = std::env::var("CODEX_STDIO_REPLAY_FIXTURE") {
        if !fixture.is_empty() {
            return Ok(Box::new(ReplayClient::new(PathBuf::from(fixture))));
        }
    }
    let binary = locate_codex_binary()?;
    Ok(Box::new(CodexExecClient::new(
        binary,
        worktree_path.to_path_buf(),
    )))
}

/// Find the `codex` CLI on disk. Search order: `CODEX_STDIO_BIN`
/// env override → `$PATH` (via `which`) → canonical
/// `/usr/bin/codex`. Returns `Err` if none of the candidates exist
/// + are executable.
pub(crate) fn locate_codex_binary() -> Result<PathBuf, String> {
    if let Ok(custom) = std::env::var("CODEX_STDIO_BIN") {
        let p = PathBuf::from(&custom);
        if is_executable(&p) {
            return Ok(p);
        }
        return Err(format!(
            "CODEX_STDIO_BIN points to `{custom}` which is not executable"
        ));
    }
    // `which codex` — short-circuits if a venv-style install is
    // ahead of /usr/bin on PATH.
    if let Ok(out) = Command::new("which").arg("codex").output() {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let p = PathBuf::from(&path);
            if !path.is_empty() && is_executable(&p) {
                return Ok(p);
            }
        }
    }
    let canonical = PathBuf::from("/usr/bin/codex");
    if is_executable(&canonical) {
        return Ok(canonical);
    }
    Err("codex CLI not found (set CODEX_STDIO_BIN or install via npm i -g @openai/codex)".into())
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;
    use std::io::Write;

    #[test]
    fn replay_client_round_trips_fixture() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let body = r#"{
            "id": "chatcmpl-test",
            "model": "gpt-5.3-codex",
            "choices": [
                { "message": { "role": "assistant", "content": "diff --git a/x b/x\n+hello\n" }, "finish_reason": "stop" }
            ],
            "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 }
        }"#;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        let req = ChatRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "fix it".into(),
            }],
            max_tokens: None,
        };
        let resp = client.chat(&req).unwrap();
        assert_eq!(resp.id, "chatcmpl-test");
        assert_eq!(resp.model, "gpt-5.3-codex");
        assert_eq!(resp.choices.len(), 1);
        assert!(resp.choices[0].message.content.contains("diff --git"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.total_tokens, 16);
    }

    #[test]
    fn replay_client_jsonl_fixture_round_trips_event_stream() {
        // .jsonl extension dispatches to replay_jsonl. Asserts thread_id +
        // all four usage axes survive the round-trip and the agent diff
        // body is extracted from item.completed.
        let mut tmp = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        let body = "{\"type\":\"thread.started\",\"thread_id\":\"thr-xyz\"}\n\
                    {\"type\":\"item.completed\",\"item\":{\"id\":\"i0\",\"type\":\"agent_message\",\"text\":\"diff --git a/x b/x\\n+hi\\n\"}}\n\
                    {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":40,\"output_tokens\":20,\"reasoning_output_tokens\":3}}\n";
        use std::io::Write as _;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        let req = ChatRequest {
            model: "gpt-5.4".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "fix it".into(),
            }],
            max_tokens: None,
        };
        let resp = client.chat(&req).unwrap();
        assert_eq!(resp.id, "thr-xyz");
        assert!(resp.choices[0].message.content.contains("diff --git"));
        let usage = resp.usage.expect("usage missing");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 120);
        assert_eq!(usage.cached_prompt_tokens, Some(40));
        assert_eq!(usage.reasoning_output_tokens, Some(3));
    }

    #[test]
    fn replay_client_json_extension_keeps_legacy_path() {
        // Mutation guard: deleting the suffix-dispatch branch would route
        // .json fixtures through replay_jsonl and break this test (Chat
        // Completions body is not a JSONL event stream).
        let mut tmp = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let body = r#"{"id":"chatcmpl-legacy","model":"gpt-5.3-codex","choices":[{"message":{"role":"assistant","content":"legacy"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        use std::io::Write as _;
        tmp.write_all(body.as_bytes()).unwrap();
        let client = ReplayClient::new(tmp.path().to_path_buf());
        let req = ChatRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![],
            max_tokens: None,
        };
        let resp = client.chat(&req).unwrap();
        assert_eq!(resp.id, "chatcmpl-legacy");
        assert_eq!(resp.choices[0].message.content, "legacy");
    }

    #[test]
    fn parse_codex_jsonl_extracts_thread_id_and_usage() {
        // Fixture mirrors a real `codex exec --json` stream: thread.started
        // first, item.completed for the agent message, turn.completed last
        // with usage. Order matters — parse must take the LAST turn.completed
        // when multiple turns occur.
        let stream = r#"{"type":"thread.started","thread_id":"019ddac0-c151-7a21-9835-52bf1417e6cf"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}
{"type":"turn.completed","usage":{"input_tokens":13171,"cached_input_tokens":11648,"output_tokens":5,"reasoning_output_tokens":0}}
"#;
        let parsed = parse_codex_jsonl(stream);
        assert_eq!(
            parsed.thread_id.as_deref(),
            Some("019ddac0-c151-7a21-9835-52bf1417e6cf")
        );
        let usage = parsed.usage.expect("turn.completed.usage missing");
        assert_eq!(usage.prompt_tokens, 13171);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 13176);
        assert_eq!(usage.cached_prompt_tokens, Some(11648));
        assert_eq!(usage.reasoning_output_tokens, Some(0));
    }

    #[test]
    fn parse_codex_jsonl_takes_last_turn_when_multiple() {
        // Multi-turn worker run: only the final tally is what gets billed.
        let stream = r#"{"type":"thread.started","thread_id":"thr-multi"}
{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}
{"type":"turn.completed","usage":{"input_tokens":250,"output_tokens":40,"cached_input_tokens":80}}
"#;
        let parsed = parse_codex_jsonl(stream);
        let usage = parsed.usage.expect("usage missing");
        assert_eq!(usage.prompt_tokens, 250);
        assert_eq!(usage.completion_tokens, 40);
        assert_eq!(usage.cached_prompt_tokens, Some(80));
    }

    #[test]
    fn parse_codex_jsonl_tolerates_noise_and_truncation() {
        // CLI warnings + stderr-merged-onto-stdout + a truncated final line.
        // Parser must skip non-JSON, must not panic, and must still return
        // partial data (thread_id present even when usage is absent).
        let stream = "warning: some Codex notice on stderr-but-merged\n\
            {\"type\":\"thread.started\",\"thread_id\":\"thr-noisy\"}\n\
            not-json garbage line\n\
            {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":50,\"output_t";
        let parsed = parse_codex_jsonl(stream);
        assert_eq!(parsed.thread_id.as_deref(), Some("thr-noisy"));
        assert!(
            parsed.usage.is_none(),
            "truncated turn.completed must not produce a usage block"
        );
    }

    #[test]
    fn parse_codex_jsonl_omits_optional_axes_when_absent() {
        // OpenAI's standard usage block (no cached_input / reasoning axes)
        // — emitted in the absence of a Codex-specific reasoning pass.
        let stream = r#"{"type":"thread.started","thread_id":"thr-min"}
{"type":"turn.completed","usage":{"input_tokens":7,"output_tokens":3}}
"#;
        let parsed = parse_codex_jsonl(stream);
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.total_tokens, 10);
        assert_eq!(usage.cached_prompt_tokens, None);
        assert_eq!(usage.reasoning_output_tokens, None);
    }

    #[test]
    fn replay_client_missing_fixture_returns_err() {
        let client = ReplayClient::new(PathBuf::from("/no/such/file.json"));
        let req = ChatRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![],
            max_tokens: None,
        };
        let err = client.chat(&req).unwrap_err();
        assert!(err.contains("read replay fixture"), "got {err:?}");
    }

    #[test]
    fn for_worktree_replay_fixture_short_circuits_binary_search() {
        let _guard = ENV_LOCK.lock().unwrap();
        // With a fixture set, for_worktree must NOT consult
        // locate_codex_binary — fixture path is the data source.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"{\"choices\":[],\"id\":\"x\",\"model\":\"\"}")
            .unwrap();
        let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
        let prev_bin = std::env::var("CODEX_STDIO_BIN").ok();
        std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", tmp.path());
        // Hostile CODEX_STDIO_BIN: if for_worktree consulted the
        // binary search, this would error. Fixture must win.
        std::env::set_var("CODEX_STDIO_BIN", "/no/such/binary/ever");
        let wt = std::path::PathBuf::from("/tmp");
        let client = match for_worktree(&wt) {
            Ok(c) => c,
            Err(e) => panic!("for_worktree failed: {e}"),
        };
        let req = ChatRequest {
            model: "gpt-5.3-codex".into(),
            messages: vec![],
            max_tokens: None,
        };
        let resp = client.chat(&req).unwrap();
        assert_eq!(resp.id, "x");
        match prev_fixture {
            Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
            None => std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE"),
        }
        match prev_bin {
            Some(v) => std::env::set_var("CODEX_STDIO_BIN", v),
            None => std::env::remove_var("CODEX_STDIO_BIN"),
        }
    }

    /// Mutation-equivalent: removing the `is_executable` check in
    /// `locate_codex_binary` would let the CODEX_STDIO_BIN override
    /// pass through unchecked; this test forces the override to a
    /// non-existent path and asserts the error.
    #[test]
    fn locate_codex_binary_rejects_nonexistent_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_fixture = std::env::var("CODEX_STDIO_REPLAY_FIXTURE").ok();
        let prev_bin = std::env::var("CODEX_STDIO_BIN").ok();
        std::env::remove_var("CODEX_STDIO_REPLAY_FIXTURE");
        std::env::set_var("CODEX_STDIO_BIN", "/no/such/binary/ever");
        let err = match locate_codex_binary() {
            Ok(p) => panic!("locate_codex_binary unexpectedly returned {p:?}"),
            Err(e) => e,
        };
        assert!(err.contains("CODEX_STDIO_BIN"), "got {err:?}");
        match prev_fixture {
            Some(v) => std::env::set_var("CODEX_STDIO_REPLAY_FIXTURE", v),
            None => {}
        }
        match prev_bin {
            Some(v) => std::env::set_var("CODEX_STDIO_BIN", v),
            None => std::env::remove_var("CODEX_STDIO_BIN"),
        }
    }

    /// Helper: build argv with stub paths + assert the resulting argv
    /// contains the strings the production code emits.
    fn argv_contains(args: &[std::ffi::OsString], needle: &str) -> bool {
        args.iter().any(|a| a.to_string_lossy() == needle)
    }

    #[test]
    fn build_codex_exec_args_default_emits_workspace_write_sandbox() {
        // bypass=false → default codex sandbox stays in place. This is
        // the production-environment path (where bwrap works).
        let args = build_codex_exec_args(
            std::path::Path::new("/tmp/wt"),
            std::path::Path::new("/tmp/out.txt"),
            "gpt-5.4",
            None,
            false,
        );
        assert!(
            argv_contains(&args, "--sandbox"),
            "missing --sandbox: {args:?}"
        );
        assert!(
            argv_contains(&args, "workspace-write"),
            "missing workspace-write: {args:?}"
        );
        assert!(
            !argv_contains(&args, "--dangerously-bypass-approvals-and-sandbox"),
            "bypass flag leaked into default path: {args:?}"
        );
    }

    #[test]
    fn build_codex_exec_args_bypass_emits_dangerous_flag_only() {
        // bypass=true → --dangerously-bypass-approvals-and-sandbox replaces
        // the --sandbox pair entirely. The codex CLI rejects passing both,
        // so the test asserts --sandbox is ABSENT in this branch.
        let args = build_codex_exec_args(
            std::path::Path::new("/tmp/wt"),
            std::path::Path::new("/tmp/out.txt"),
            "gpt-5.4",
            None,
            true,
        );
        assert!(
            argv_contains(&args, "--dangerously-bypass-approvals-and-sandbox"),
            "missing bypass flag: {args:?}"
        );
        assert!(
            !argv_contains(&args, "--sandbox"),
            "--sandbox flag must NOT coexist with bypass: {args:?}"
        );
        assert!(
            !argv_contains(&args, "workspace-write"),
            "workspace-write value must NOT coexist with bypass: {args:?}"
        );
    }

    #[test]
    fn sandbox_bypass_from_env_unset_returns_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CODEX_STDIO_SANDBOX_BYPASS").ok();
        std::env::remove_var("CODEX_STDIO_SANDBOX_BYPASS");
        assert!(!sandbox_bypass_from_env());
        if let Some(v) = prev {
            std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", v);
        }
    }

    #[test]
    fn sandbox_bypass_from_env_explicit_zero_returns_false() {
        // Boundary: explicit "0" must NOT trip bypass — operator who set
        // the var to disable bypass should not get bypass anyway.
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CODEX_STDIO_SANDBOX_BYPASS").ok();
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "0");
        assert!(!sandbox_bypass_from_env(), "0 must not enable bypass");
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "false");
        assert!(!sandbox_bypass_from_env(), "false must not enable bypass");
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "");
        assert!(!sandbox_bypass_from_env(), "empty must not enable bypass");
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "garbage");
        assert!(!sandbox_bypass_from_env(), "garbage must not enable bypass");
        match prev {
            Some(v) => std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", v),
            None => std::env::remove_var("CODEX_STDIO_SANDBOX_BYPASS"),
        }
    }

    #[test]
    fn sandbox_bypass_from_env_truthy_returns_true() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CODEX_STDIO_SANDBOX_BYPASS").ok();
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "1");
        assert!(sandbox_bypass_from_env(), "1 must enable bypass");
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "true");
        assert!(sandbox_bypass_from_env(), "true must enable bypass");
        std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", "TRUE");
        assert!(sandbox_bypass_from_env(), "TRUE (case) must enable bypass");
        match prev {
            Some(v) => std::env::set_var("CODEX_STDIO_SANDBOX_BYPASS", v),
            None => std::env::remove_var("CODEX_STDIO_SANDBOX_BYPASS"),
        }
    }

    #[test]
    fn build_codex_exec_args_max_tokens_round_trips() {
        // Glue-but-load-bearing: max_tokens=Some(N) appends the -c
        // model_max_output_tokens=N pair. Mutation: deleting the
        // if-let branch breaks this test.
        let args = build_codex_exec_args(
            std::path::Path::new("/tmp/wt"),
            std::path::Path::new("/tmp/out.txt"),
            "gpt-5.4",
            Some(1024),
            false,
        );
        assert!(argv_contains(&args, "-c"), "missing -c: {args:?}");
        assert!(
            argv_contains(&args, "model_max_output_tokens=1024"),
            "missing token-cap kv: {args:?}"
        );
    }
}
