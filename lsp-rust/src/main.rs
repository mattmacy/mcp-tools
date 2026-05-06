//! lsp-rust — long-lived LSP shim around rust-analyzer.
//!
//! Replaces `zeenix/rust-analyzer-mcp` v0.2.0 (see the README for the
//! full bug-fix matrix). Exposes the same handful of operations agents
//! actually use (definition, references, hover, workspace_symbols,
//! diagnostics, wait-for-indexing) over two surfaces:
//!
//! 1. **CLI** for shell-level debugging:
//!    `lsp-rust definition <file>:<line>:<col>`
//!    `lsp-rust references <file>:<line>:<col>`
//!    `lsp-rust hover <file>:<line>:<col>`
//!    `lsp-rust workspace-symbols <query>`
//!    `lsp-rust diagnostics <file>`
//!    `lsp-rust wait-for-indexing`
//!
//! 2. **MCP stdio server** for agent prompt ergonomics:
//!    `lsp-rust serve-mcp`
//!
//! In both modes the same long-lived `rust-analyzer` subprocess answers
//! every request — no per-call respawn, indexing cost paid once on startup.

#![deny(missing_docs)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use lsp_rust::compat::project_env;
use lsp_rust::lsp::{LspShimError, RustAnalyzerClient};
use lsp_rust::mcp;

/// Top-level CLI surface. Every leaf subcommand runs the same long-lived
/// rust-analyzer subprocess; only the dispatched LSP request differs.
#[derive(Parser, Debug)]
#[command(
    name = "lsp-rust",
    version,
    about = "thin LSP shim around rust-analyzer."
)]
struct Cli {
    /// Workspace root to initialize rust-analyzer against. Defaults to the
    /// `LSP_PROJECT` env var, then the current working directory at
    /// invocation time.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,

    /// Per-request timeout in seconds. Overridable via `LSP_TIMEOUT_SECS`
    /// (CLI flag wins). Default 60s — three rounds of 30s zeenix-wrapper
    /// timeouts on indexing-heavy reqs is what motivated this shim.
    #[arg(long, global = true)]
    timeout_secs: Option<u64>,

    /// JSON log file path. Default `/tmp/lsp-rust.log`. Override via
    /// `LSP_LOG_FILE`.
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// One subcommand per LSP operation we expose, plus `serve-mcp` for the MCP
/// stdio server mode.
#[derive(Subcommand, Debug)]
enum Command {
    /// `textDocument/definition` — go-to-definition at file:line:col.
    Definition {
        /// `path/to/file.rs:LINE:COL` — line/col 1-based to match editors.
        location: String,
    },
    /// `textDocument/references` — find references at file:line:col.
    References {
        /// `path/to/file.rs:LINE:COL`.
        location: String,
    },
    /// `textDocument/hover` — hover info at file:line:col.
    Hover {
        /// `path/to/file.rs:LINE:COL`.
        location: String,
    },
    /// `workspace/symbol` — fuzzy symbol search across the workspace.
    WorkspaceSymbols {
        /// Query string forwarded verbatim.
        query: String,
    },
    /// `textDocument/diagnostic` — current diagnostic set for one file.
    Diagnostics {
        /// File path.
        file: PathBuf,
    },
    /// Block until rust-analyzer finishes indexing. Replaces the zeenix
    /// wrapper's hardcoded poll-substring scan with explicit polling
    /// of `workspace/symbol` until two successive samples digest-match.
    WaitForIndexing,
    /// MCP stdio server mode. Exposes every other subcommand as a tool.
    ServeMcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(path) = &cli.log_file {
        std::env::set_var("LSP_LOG_FILE", path);
    }

    let workspace = cli
        .workspace
        .clone()
        .or_else(|| project_env().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok());
    let workspace = match workspace {
        Some(w) => w,
        None => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "error": "no workspace root resolvable; pass --workspace or set LSP_PROJECT",
                    "kind": "internal",
                })
            );
            return ExitCode::from(2);
        }
    };

    match cli.command {
        Command::ServeMcp => match mcp::serve_stdio(workspace, cli.timeout_secs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": format!("{e}"), "kind": "io"})
                );
                ExitCode::from(1)
            }
        },
        other => {
            let mut backend = RustAnalyzerClient::new(workspace, cli.timeout_secs);
            let result = run_subcommand(&mut backend, other);
            match result {
                Ok(value) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({"error": format!("{e}"), "kind": error_kind(&e)})
                    );
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn run_subcommand(
    backend: &mut RustAnalyzerClient,
    cmd: Command,
) -> std::result::Result<serde_json::Value, LspShimError> {
    match cmd {
        Command::Definition { location } => {
            let (file, line, column) = parse_location(&location)?;
            backend.definition(&file, line, column)
        }
        Command::References { location } => {
            let (file, line, column) = parse_location(&location)?;
            backend.references(&file, line, column)
        }
        Command::Hover { location } => {
            let (file, line, column) = parse_location(&location)?;
            backend.hover(&file, line, column)
        }
        Command::WorkspaceSymbols { query } => backend.workspace_symbols(&query),
        Command::Diagnostics { file } => backend.diagnostics(&file),
        Command::WaitForIndexing => backend.wait_for_indexing(),
        Command::ServeMcp => unreachable!("serve-mcp handled above"),
    }
}

/// Parse `path/to/file.rs:LINE:COL` into its three parts. 1-based line and
/// column to match editor conventions; the LSP layer subtracts 1 before
/// shipping to rust-analyzer.
fn parse_location(s: &str) -> std::result::Result<(PathBuf, u32, u32), LspShimError> {
    // Right-split twice so file paths containing `:` (rare on Linux but
    // legal) survive.
    let (rest, col_s) = s.rsplit_once(':').ok_or_else(|| {
        LspShimError::Protocol(format!("bad location {s:?}: expected file:line:col"))
    })?;
    let (file_s, line_s) = rest
        .rsplit_once(':')
        .ok_or_else(|| LspShimError::Protocol(format!("bad location {s:?}: missing line")))?;
    let line: u32 = line_s
        .parse()
        .map_err(|e| LspShimError::Protocol(format!("bad line {line_s:?}: {e}")))?;
    let column: u32 = col_s
        .parse()
        .map_err(|e| LspShimError::Protocol(format!("bad column {col_s:?}: {e}")))?;
    Ok((PathBuf::from(file_s), line, column))
}

fn error_kind(e: &LspShimError) -> &'static str {
    match e {
        LspShimError::Spawn { .. } => "spawn",
        LspShimError::Lsp { .. } => "lsp",
        LspShimError::Timeout { .. } => "timeout",
        LspShimError::Protocol(_) => "protocol",
        LspShimError::Io(_) => "io",
        LspShimError::Json(_) => "json",
        LspShimError::Internal(_) => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_location_extracts_three_parts() {
        let (f, l, c) = parse_location("/tmp/foo.rs:42:7").expect("parses");
        assert_eq!(f, PathBuf::from("/tmp/foo.rs"));
        assert_eq!(l, 42);
        assert_eq!(c, 7);
    }

    #[test]
    fn parse_location_rejects_missing_column() {
        let err = parse_location("/tmp/foo.rs:42").expect_err("must fail");
        assert!(matches!(err, LspShimError::Protocol(_)));
    }

    #[test]
    fn parse_location_rejects_non_numeric_line() {
        let err = parse_location("/tmp/foo.rs:abc:7").expect_err("must fail");
        assert!(matches!(err, LspShimError::Protocol(_)));
    }
}
