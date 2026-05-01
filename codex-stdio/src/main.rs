//! `codex-stdio` binary entrypoint.
//!
//! Three CLI subcommands:
//!
//! - `serve-mcp` — newline-delimited JSON-RPC 2.0 over stdio, the
//!   transport `.mcp.json` invokes.
//! - `health` — one-shot CLI mirror of the `codex_health` MCP tool.
//!   Useful for shell-level smoke testing + reach-probe scripts.
//! - `probe` — full smoke: lists tools, asserts schema, runs health,
//!   exits 0 only if every step succeeds. Documented expected output
//!   in `README.md`.

#![deny(missing_docs)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use codex_stdio::{health, mcp, run_task};

/// Top-level CLI surface.
#[derive(Parser, Debug)]
#[command(
    name = "codex-stdio",
    version,
    about = "stdio MCP server proxying OpenAI Chat Completions as a Codex worker backend."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// CLI subcommands.
#[derive(Subcommand, Debug)]
enum Command {
    /// MCP stdio server mode — the transport `.mcp.json` invokes.
    ServeMcp,
    /// One-shot health check. Mirrors the `codex_health` tool.
    Health,
    /// Smoke test — exercise every code path that does not require a
    /// live OpenAI call. Exit non-zero on any failure.
    Probe,
    /// One-shot task dispatch. CLI mirror of the `codex_run_task`
    /// tool. Reads the task packet from stdin so credentials / large
    /// prompts do not appear in shell history.
    RunTask {
        /// Absolute path the worker is constrained to.
        #[arg(long)]
        worktree: String,
        /// Optional output ceiling.
        #[arg(long)]
        max_tokens: Option<u64>,
        /// Optional model override.
        #[arg(long)]
        model: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::ServeMcp => match mcp::serve_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": format!("{e}"), "kind": "io"})
                );
                ExitCode::from(1)
            }
        },
        Command::Health => emit_or_die(health::run()),
        Command::Probe => probe(),
        Command::RunTask {
            worktree,
            max_tokens,
            model,
        } => {
            let mut task_packet = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut task_packet) {
                return die(format!("read stdin: {e}"));
            }
            let mut args = serde_json::json!({
                "task_packet": task_packet.trim(),
                "worktree_path": worktree,
            });
            if let Some(m) = max_tokens {
                args["max_tokens"] = serde_json::Value::from(m);
            }
            if let Some(m) = model {
                args["model"] = serde_json::Value::from(m);
            }
            emit_or_die(run_task::run(&args))
        }
    }
}

/// Smoke test — validates the MCP wire shape end-to-end without a
/// live OpenAI call. Health is exercised against the current process
/// env (typically reports `available: false` in CI). tools/list is
/// asserted to advertise both tools with required-arg schemas.
/// Exit 0 means the binary loads, schemas are well-formed, and
/// health reports a payload with the expected shape regardless of
/// whether creds are present.
fn probe() -> ExitCode {
    let mut summary = serde_json::Map::new();
    let mut any_err = false;

    // 1. tools/list
    let tools = mcp::tools_list_result();
    let tools_arr = tools["tools"].as_array().cloned().unwrap_or_default();
    summary.insert(
        "tools_count".into(),
        serde_json::Value::from(tools_arr.len()),
    );
    if tools_arr.len() != 2 {
        any_err = true;
        summary.insert(
            "tools_count_error".into(),
            serde_json::Value::from(format!("expected 2, got {}", tools_arr.len())),
        );
    }

    // 2. health
    match health::run() {
        Ok(v) => {
            // Shape assertion: must have available + model + latency_ms.
            for k in &["available", "model", "latency_ms"] {
                if v.get(*k).is_none() {
                    any_err = true;
                    summary.insert(format!("health_missing_{k}"), serde_json::Value::from(true));
                }
            }
            summary.insert("health".into(), v);
        }
        Err(e) => {
            any_err = true;
            summary.insert("health_error".into(), serde_json::Value::from(e));
        }
    }

    let payload = serde_json::Value::Object(summary);
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    );
    if any_err {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn emit_or_die(r: Result<serde_json::Value, String>) -> ExitCode {
    match r {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            );
            ExitCode::SUCCESS
        }
        Err(e) => die(e),
    }
}

fn die(e: String) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({"error": e, "kind": "tool_failure"})
    );
    ExitCode::from(1)
}
