#!/usr/bin/env python3
"""
LSP driver that keeps clangd running and feeds it work to index.

clangd's background indexer is queue-driven: it crawls TUs whose compile
commands are reachable from currently-open files (the file plus its
include closure). With no files open, the queue is empty and clangd sits
idle at 0% CPU.

This driver sends `initialize` + `initialized`, parses
compile_commands.json, then sends a `textDocument/didOpen` for each TU
(throttled — we open files in batches and let clangd close them). The
background indexer queues the closure of every opened TU and, given
enough opens, ends up indexing essentially the whole compilation
database. We open TUs slowly so clangd's worker queue doesn't grow
unbounded.

Run via start-clangd.sh; not intended to be invoked manually.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import threading
import time

PROJECT_ROOT = os.environ.get("PROJECT_ROOT") or os.environ.get("UE_ROOT") or ""
if not PROJECT_ROOT:
    sys.stderr.write("error: PROJECT_ROOT not set\n")
    sys.exit(2)
LOG_FILE = os.environ.get("CLANGD_LOG", "/tmp/clangd.log")
CLANGD = os.environ.get("CLANGD_BIN", "clangd")
# clangd worker count. Large C++ TUs may have 50-200 MB working sets each.
# Default to half the CPU count to avoid oversubscription / paging during
# PCH thrash. Override with CLANGD_JOBS.
CLANGD_JOBS = os.environ.get("CLANGD_JOBS", str(max(2, (os.cpu_count() or 4) // 2)))
# Heartbeat cadence for the watchdog that tracks whether clangd is still
# making forward progress. The watchdog never kills clangd — it only
# annotates the log so wrappers consuming the log can distinguish
# "indexing in progress" from "process crashed".
HEARTBEAT_INTERVAL_S = int(os.environ.get("CLANGD_HEARTBEAT_S", "60"))


def lsp_send(stream, payload: dict) -> None:
    body = json.dumps(payload).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    stream.write(header + body)
    stream.flush()


def drain_stdout(proc: subprocess.Popen) -> None:
    """Discard clangd's LSP stdout responses; we only care about stderr (the log)."""
    while True:
        chunk = proc.stdout.read(4096)
        if not chunk:
            return


def main() -> int:
    log = open(LOG_FILE, "ab", buffering=0)
    bg_mem_limit = os.environ.get("CLANGD_INDEX_MEM_LIMIT", "").strip()
    clangd_argv = [
        CLANGD,
        "--background-index",
        f"--compile-commands-dir={PROJECT_ROOT}",
        "--log=info",
        "--pch-storage=disk",
        f"-j={CLANGD_JOBS}",
        # Yield to interactive (foreground) requests when both compete
        # for CPU. Background indexing still progresses but won't
        # starve a query from MCP server / a Claude Code MCP call.
        "--background-index-priority=background",
        # project's IWYU is non-standard; clangd's auto-insertion is wrong
        # often enough that the false-positive rate exceeds the
        # value. Disable to keep query responses focused on what was
        # asked.
        "--header-insertion=never",
        # Disable clang-tidy passes — the project TUs are huge and the per-TU
        # tidy traversal is the second-largest memory consumer after
        # the merged background index. The MCP cpp bridge does not
        # surface tidy diagnostics, so the cost is unredeemed.
        "--clang-tidy=false",
    ]
    # shim-local: cap on resident merged-index memory. Only the
    # patched clangd understands this flag; stock clangd-19 exits
    # on unknown flag, so only inject when CLANGD path resembles
    # `clangd-patched`.
    if bg_mem_limit and "clangd-patched" in CLANGD:
        clangd_argv.append(f"--background-index-memory-limit={bg_mem_limit}")
    # shim-local: cap simultaneous preamble builds (decouples preamble
    # concurrency from -j). Default 6 picked for the workspace box: -j=10
    # peaked ~40 GB unbounded; --max-concurrent-preamble-builds=6 lands
    # peak around 24-28 GB while keeping the post-preamble indexer fully
    # parallel. Override via CLANGD_MAX_PREAMBLE_BUILDS.
    preamble_cap = os.environ.get("CLANGD_MAX_PREAMBLE_BUILDS", "6").strip()
    if preamble_cap and preamble_cap != "0" and "clangd-patched" in CLANGD:
        clangd_argv.append(f"--max-concurrent-preamble-builds={preamble_cap}")
    proc = subprocess.Popen(
        clangd_argv,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=log,
    )

    drain_thread = threading.Thread(target=drain_stdout, args=(proc,), daemon=True)
    drain_thread.start()

    pid_dir = os.path.dirname(LOG_FILE) or "/tmp"
    pid_file = os.environ.get("CLANGD_PID_FILE", os.path.join(pid_dir, "clangd.pid"))
    with open(pid_file + ".clangd", "w") as f:
        f.write(str(proc.pid))

    # Forward SIGTERM/SIGINT to clangd so the parent shutdown is clean.
    def shutdown(_signum, _frame):
        try:
            proc.terminate()
        except Exception:
            pass
        proc.wait(timeout=10)
        sys.exit(0)

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    # LSP initialize. rootUri must be a file:// URI of the project.
    lsp_send(
        proc.stdin,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": os.getpid(),
                "rootUri": f"file://{PROJECT_ROOT}",
                "capabilities": {},
                "initializationOptions": {},
            },
        },
    )
    # initialized notification kicks the background indexer.
    lsp_send(
        proc.stdin,
        {
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {},
        },
    )

    # Feed clangd work: open every TU in compile_commands.json so the
    # background indexer queues each one's include closure. We rotate
    # through batches so memory stays bounded — clangd evicts ASTs of
    # closed files but keeps the index entries it built.
    #
    # Prefer a narrow compilation DB if one exists (e.g. a hand-curated
    # subset of TUs covering only the subsystems you care about). Falls
    # back to the full DB if the narrow file isn't present.
    narrow_path = os.path.join(PROJECT_ROOT, "compile_commands.narrow.json")
    full_path = os.path.join(PROJECT_ROOT, "compile_commands.json")
    if os.path.isfile(narrow_path):
        cdb_path = narrow_path
        log.write(b"\n[driver] using narrow compile_commands.narrow.json\n")
    else:
        cdb_path = full_path
        log.write(
            b"\n[driver] narrow DB missing; falling back to full "
            b"compile_commands.json\n"
        )
    try:
        with open(cdb_path, "r", encoding="utf-8") as f:
            cdb = json.load(f)
    except Exception as e:
        log.write(f"\n[driver] failed to read {cdb_path}: {e}\n".encode())
        cdb = []

    files = []
    seen = set()
    for entry in cdb:
        path = entry.get("file") or ""
        if not path or path in seen:
            continue
        seen.add(path)
        files.append(path)

    log.write(
        f"\n[driver] {len(files)} unique TUs in compile_commands.json; opening in batches of 32\n".encode()
    )

    # Heartbeat state shared between the TU-feeding loop and the
    # post-feed hold loop. The first feeding pass takes ~ len(files)/32 *
    # 20s wall (~35 min for 3 329 narrow TUs); without interleaving the
    # heartbeat into the feeding loop, no marker line appears until that
    # finishes. Consumers tailing the log would see "no heartbeat for
    # 35 min" and reasonably assume the wrapper is dead.
    last_heartbeat = time.monotonic()
    last_log_size = os.path.getsize(LOG_FILE) if os.path.isfile(LOG_FILE) else 0

    def maybe_heartbeat() -> None:
        nonlocal last_heartbeat, last_log_size
        now = time.monotonic()
        if now - last_heartbeat < HEARTBEAT_INTERVAL_S:
            return
        last_heartbeat = now
        try:
            current_log_size = os.path.getsize(LOG_FILE)
        except OSError:
            current_log_size = last_log_size
        delta = current_log_size - last_log_size
        last_log_size = current_log_size
        status = "progressing" if delta > 0 else "quiescent"
        marker = (
            f"\n[driver heartbeat] clangd pid={proc.pid} status={status} "
            f"log_delta_bytes={delta}\n"
        ).encode()
        try:
            log.write(marker)
        except OSError:
            pass

    BATCH = 32
    next_doc_id = 100
    open_uris: list[str] = []
    idx = 0
    while idx < len(files) and proc.poll() is None:
        # Close previous batch.
        for uri in open_uris:
            lsp_send(
                proc.stdin,
                {
                    "jsonrpc": "2.0",
                    "method": "textDocument/didClose",
                    "params": {"textDocument": {"uri": uri}},
                },
            )
        open_uris = []
        # Open next batch.
        end = min(idx + BATCH, len(files))
        for path in files[idx:end]:
            uri = f"file://{path}"
            # didOpen requires text — but clangd accepts empty text and
            # reads from disk via the compile command anyway for indexing
            # purposes. For files generated under Intermediate/, attempt
            # a real read; fall back to empty text if the file doesn't
            # exist on disk (UHT-generated TUs may be transient).
            try:
                with open(path, "r", encoding="utf-8", errors="replace") as f:
                    text = f.read()
            except OSError:
                text = ""
            lsp_send(
                proc.stdin,
                {
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
                },
            )
            open_uris.append(uri)
            next_doc_id += 1
        idx = end
        # Pause so clangd can process the batch before we open more.
        time.sleep(20)
        maybe_heartbeat()

    log.write(b"\n[driver] all TUs queued; holding connection open for ongoing indexing\n")

    # Hold the connection open indefinitely so clangd keeps the index
    # cache writable. Background work continues even after didClose for
    # already-queued TUs.
    #
    # Heartbeat watchdog: every HEARTBEAT_INTERVAL_S seconds, write a
    # marker line to the log noting the wrapper is still alive and
    # carrying clangd. Consumers tailing the log (MCP server's
    # log_monitor, operators) can use these marker lines to distinguish:
    #   - "clangd progressing"  → fresh ASTWorker/Indexed entries between markers
    #   - "clangd quiescent"    → markers but no ASTWorker entries (idle, fine)
    #   - "wrapper dead"        → no markers
    # We deliberately do NOT kill clangd from the watchdog. Bug 2 root
    # cause was wrappers killing clangd on perceived unresponsiveness +
    # then exiting silently, leaving downstream MCP consumers with no
    # signal at all. Logging a degraded marker preserves the connection
    # while still alerting consumers.
    while proc.poll() is None:
        time.sleep(min(HEARTBEAT_INTERVAL_S, 5))
        maybe_heartbeat()

    return proc.returncode or 0


if __name__ == "__main__":
    sys.exit(main())
