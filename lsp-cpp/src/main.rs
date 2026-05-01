//! `lsp-cpp` CLI.
//!
//! One-shot subcommands intended for direct use by humans and by an
//! eventual MCP layer (the MCP wrapper will translate
//! `mcp__lsp-cpp__definition` → `lsp-cpp definition …`).
//!
//! Until the MCP wrapper lands, the CLI is also the integration target
//! for the `LspBackend` trait — every backend method has a matching
//! subcommand.
//!
//! Subcommands:
//!
//! - `lsp-cpp workspace-symbol <query>`
//! - `lsp-cpp definition <file> <line> <column>`
//! - `lsp-cpp references <file> <line> <column>`
//! - `lsp-cpp hover <file> <line> <column>`
//! - `lsp-cpp probe` — spawns clangd, runs `initialize`, then
//!   `workspace/symbol FFrame`, prints the first hit, exits. Used by
//!   the integration test on hosts where clangd-19 is installed.
//!
//! Project root is taken from `--project <path>` or `LSP_PROJECT`
//! (default `/path/to/project`).

use lsp_cpp::{
    clangd::{Clangd, IndexMode, BUILD_FULL_INDEX_DEFAULT_MAX_TUS},
    mcp, LspBackend, ShimError,
};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{}", USAGE);
        return ExitCode::from(2);
    }

    let mut project: String = std::env::var("LSP_PROJECT")
        .unwrap_or_else(|_| "/path/to/project".into());
    let mut mode_override: Option<IndexMode> = None;
    let mut index_file_override: Option<PathBuf> = None;
    let mut max_tus_override: Option<usize> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => {
                project = iter.next().unwrap_or_default();
            }
            "--max-tus" => {
                let v = iter.next().unwrap_or_default();
                match v.parse::<usize>() {
                    Ok(n) if n > 0 => max_tus_override = Some(n),
                    _ => {
                        eprintln!("--max-tus expects a positive integer, got {v:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--mode" => {
                let v = iter.next().unwrap_or_default();
                mode_override = Some(match v.as_str() {
                    "narrow" => IndexMode::Narrow,
                    "full" => IndexMode::Full {
                        index_file: PathBuf::new(),
                    },
                    "hybrid" => IndexMode::Hybrid {
                        index_file: PathBuf::new(),
                    },
                    other => {
                        eprintln!("unknown --mode {other:?} (expected narrow|full|hybrid)");
                        return ExitCode::from(2);
                    }
                });
            }
            "--index-file" => {
                index_file_override = Some(PathBuf::from(iter.next().unwrap_or_default()));
            }
            "-h" | "--help" => {
                println!("{}", USAGE);
                return ExitCode::SUCCESS;
            }
            _ => positional.push(arg),
        }
    }

    let cmd = match positional.first() {
        Some(c) => c.clone(),
        None => {
            eprintln!("{}", USAGE);
            return ExitCode::from(2);
        }
    };
    let rest: Vec<String> = positional.into_iter().skip(1).collect();

    // Resolve the effective index mode. CLI `--mode` overrides env
    // (`LSP_CPP_INDEX_MODE`); CLI `--index-file` overrides the
    // path baked in by env / default.
    let mut backend = Clangd::new(&project);
    if let Some(mut mode) = mode_override {
        if let Some(path) = index_file_override.clone() {
            mode = match mode {
                IndexMode::Narrow => IndexMode::Narrow,
                IndexMode::Full { .. } => IndexMode::Full { index_file: path },
                IndexMode::Hybrid { .. } => IndexMode::Hybrid { index_file: path },
            };
        } else if matches!(
            mode,
            IndexMode::Full { ref index_file, .. } | IndexMode::Hybrid { ref index_file, .. }
            if index_file.as_os_str().is_empty()
        ) {
            // --mode full|hybrid without --index-file: fall back to
            // the env-derived default the same way `IndexMode::from_env`
            // does.
            let default_mode = IndexMode::from_env();
            mode = match (mode, default_mode) {
                (
                    IndexMode::Full { .. },
                    IndexMode::Full { index_file } | IndexMode::Hybrid { index_file },
                ) => IndexMode::Full { index_file },
                (
                    IndexMode::Hybrid { .. },
                    IndexMode::Full { index_file } | IndexMode::Hybrid { index_file },
                ) => IndexMode::Hybrid { index_file },
                (IndexMode::Full { .. }, _) => IndexMode::Full {
                    index_file: PathBuf::from(default_index_file()),
                },
                (IndexMode::Hybrid { .. }, _) => IndexMode::Hybrid {
                    index_file: PathBuf::from(default_index_file()),
                },
                _ => IndexMode::Narrow,
            };
        }
        backend = backend.with_index_mode(mode);
    }

    let result: Result<(), ShimError> = match cmd.as_str() {
        "probe" => run_probe(&mut backend),
        "workspace-symbol" => run_workspace_symbol(&mut backend, &rest),
        "definition" => run_position_query(&mut backend, &rest, QueryKind::Definition),
        "references" => run_position_query(&mut backend, &rest, QueryKind::References),
        "hover" => run_position_query(&mut backend, &rest, QueryKind::Hover),
        "build-full-index" => run_build_full_index(&mut backend, max_tus_override),
        "serve-mcp" => run_serve_mcp(backend),
        other => {
            eprintln!("unknown subcommand: {other}\n\n{}", USAGE);
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Structured error → JSON on stderr so callers can parse if
            // they want to, plus a one-line summary on the last line.
            let json = serde_json::json!({
                "error": format!("{e}"),
                "kind": error_kind(&e),
            });
            eprintln!("{}", serde_json::to_string(&json).unwrap_or_default());
            ExitCode::from(1)
        }
    }
}

const USAGE: &str = "\
lsp-cpp — thin LSP shim around clangd-19.

USAGE:
    lsp-cpp [--project <path>] <subcommand> [args]

SUBCOMMANDS:
    probe                                    spawn clangd, search for FFrame
    workspace-symbol <query>                 workspace-wide symbol search
    definition <file> <line> <column>        go-to-definition
    references <file> <line> <column>        find references
    hover <file> <line> <column>             hover documentation
    build-full-index [--max-tus N]           pre-warm clangd's persistent index
                                             by feeding the first N TUs from
                                             compile_commands.json (default
                                             5000). Blocking; tail
                                             /tmp/clangd.log for progress.
    serve-mcp                                run MCP (Model Context Protocol)
                                             stdio server; consumed by Claude
                                             Code's MCP host as the `lsp-cpp` entry

OPTIONS:
    --project <path>           project root (default /path/to/project)
    --mode narrow|full|hybrid  indexing strategy (default narrow)
    --index-file <path>        pre-built index file for full / hybrid mode
                               (default $HOME/.cache/lsp-cpp-full-index/index.idx)
    --max-tus <n>              cap for build-full-index (default 5000)

ENV:
    LSP_PROJECT          project root containing compile_commands.json
                              (default /path/to/project)
    LSP_CPP_INDEX_MODE   narrow | full | hybrid (default narrow)
    LSP_CPP_INDEX_FILE   pre-built index path for full / hybrid mode
    CLANGD_BIN                clangd binary (default clangd-19)
    CLANGD_LOG                clangd stderr log (default /tmp/clangd.log)
    CLANGD_JOBS               clangd worker count (default: half of nproc, min 2)
";

enum QueryKind {
    Definition,
    References,
    Hover,
}

fn run_probe(backend: &mut Clangd) -> Result<(), ShimError> {
    backend.spawn()?;
    let symbols = backend.workspace_symbol("FFrame")?;
    let json = serde_json::json!({
        "probe": "FFrame",
        "hits": symbols.len(),
        "first": symbols.first(),
    });
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

fn run_workspace_symbol(backend: &mut Clangd, args: &[String]) -> Result<(), ShimError> {
    let query = args.first().cloned().unwrap_or_default();
    if query.is_empty() {
        return Err(ShimError::Protocol(
            "workspace-symbol requires a query".into(),
        ));
    }
    let symbols = backend.workspace_symbol(&query)?;
    println!("{}", serde_json::to_string_pretty(&symbols)?);
    Ok(())
}

fn run_position_query(
    backend: &mut Clangd,
    args: &[String],
    kind: QueryKind,
) -> Result<(), ShimError> {
    if args.len() < 3 {
        return Err(ShimError::Protocol(
            "expected <file> <line> <column>".into(),
        ));
    }
    let file = &args[0];
    let line: u32 = args[1]
        .parse()
        .map_err(|e| ShimError::Protocol(format!("bad line {:?}: {e}", args[1])))?;
    let column: u32 = args[2]
        .parse()
        .map_err(|e| ShimError::Protocol(format!("bad column {:?}: {e}", args[2])))?;
    match kind {
        QueryKind::Definition => {
            let locs = backend.definition(file, line, column)?;
            println!("{}", serde_json::to_string_pretty(&locs)?);
        }
        QueryKind::References => {
            let locs = backend.references(file, line, column)?;
            println!("{}", serde_json::to_string_pretty(&locs)?);
        }
        QueryKind::Hover => {
            let hov = backend.hover(file, line, column)?;
            println!("{}", serde_json::to_string_pretty(&hov)?);
        }
    }
    Ok(())
}

fn error_kind(e: &ShimError) -> &'static str {
    match e {
        ShimError::ClangdMissing { .. } => "clangd_missing",
        ShimError::NoCompileCommands { .. } => "no_compile_commands",
        ShimError::NoIndexFile { .. } => "no_index_file",
        ShimError::InitializeTimeout { .. } => "initialize_timeout",
        ShimError::RequestTimeout { .. } => "request_timeout",
        ShimError::ClangdBusy { .. } => "clangd_busy",
        ShimError::QueueDepthExceeded { .. } => "queue_depth_exceeded",
        ShimError::ClangdExited { .. } => "clangd_exited",
        ShimError::Protocol(_) => "protocol",
        ShimError::Io(_) => "io",
        ShimError::Json(_) => "json",
    }
}

fn default_index_file() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    format!("{home}/.cache/lsp-cpp-full-index/index.idx")
}

fn run_serve_mcp(backend: Clangd) -> Result<(), ShimError> {
    // Owned backend hands off to the MCP loop, which holds it for the
    // lifetime of the stdio session (one long-lived clangd subprocess
    // per MCP session — see mcp.rs module docstring).
    mcp::serve_stdio(backend).map_err(ShimError::Io)
}

fn run_build_full_index(
    backend: &mut Clangd,
    max_tus_override: Option<usize>,
) -> Result<(), ShimError> {
    // Pre-warm clangd's persistent index in-process. Mirrors the MCP
    // `build_full_index` tool. Default cap matches the MCP default
    // (BUILD_FULL_INDEX_DEFAULT_MAX_TUS) so behaviour is identical
    // across both transports.
    let max_tus = max_tus_override.unwrap_or(BUILD_FULL_INDEX_DEFAULT_MAX_TUS);
    let report = backend.build_full_index(max_tus)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
