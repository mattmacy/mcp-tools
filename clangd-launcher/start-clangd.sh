#!/usr/bin/env bash
# Launch clangd in background-index mode against a large C++ workspace.
# Idempotent: bails if a clangd is already running with --compile-commands-dir
# pointing at the same project.
set -euo pipefail

# Project root with a generated compile_commands.json. Override via env.
PROJECT_ROOT="${PROJECT_ROOT:-${UE_ROOT:-}}"
if [[ -z "${PROJECT_ROOT}" ]]; then
  echo "error: set PROJECT_ROOT (or UE_ROOT) to the directory containing compile_commands.json" >&2
  exit 2
fi

LOG_FILE="${CLANGD_LOG:-/tmp/clangd.log}"
PID_FILE="${CLANGD_PID_FILE:-/tmp/clangd.pid}"

# Prefer a patched clangd if installed (clangd-19 base + the
# `--background-index-memory-limit` flag added by build-clangd-patched.sh).
# Fall back to clangd-19, then to whatever `clangd` resolves to.
# clangd-14 (Debian default) crashes parsing some C++20 extensions so
# avoid it where possible.
if command -v clangd-patched >/dev/null 2>&1; then
  CLANGD_BIN="$(command -v clangd-patched)"
elif command -v clangd-19 >/dev/null 2>&1; then
  CLANGD_BIN="$(command -v clangd-19)"
else
  CLANGD_BIN="$(command -v clangd)"
fi
export CLANGD_BIN

# Default resident-memory cap for the merged background index. Only
# clangd-patched honors this; stock clangd silently ignores the unknown
# flag at startup, so this is a safe no-op until the patched binary is
# installed.
export CLANGD_INDEX_MEM_LIMIT="${CLANGD_INDEX_MEM_LIMIT:-10G}"

# Cap simultaneous preamble builds (clangd-patched only). Each in-flight
# preamble holds a full Clang AST + temporary buffers, which can run
# multiple GB on large translation units.
export CLANGD_MAX_PREAMBLE_BUILDS="${CLANGD_MAX_PREAMBLE_BUILDS:-3}"

# LD_PRELOAD jemalloc (libjemalloc.so.2). jemalloc with dirty_decay_ms=0
# returns freed pages to the OS aggressively instead of holding them in
# arenas the way glibc malloc does, cutting clangd RSS by 10-25% on
# preamble-heavy workloads. No-op if the package is missing.
JEMALLOC_LIB="/usr/lib/x86_64-linux-gnu/libjemalloc.so.2"
if [[ -f "${JEMALLOC_LIB}" ]]; then
  export LD_PRELOAD="${JEMALLOC_LIB}${LD_PRELOAD:+:${LD_PRELOAD}}"
  export MALLOC_CONF="${MALLOC_CONF:-dirty_decay_ms:0,muzzy_decay_ms:0}"
fi

if [[ ! -f "${PROJECT_ROOT}/compile_commands.json" ]]; then
  echo "error: ${PROJECT_ROOT}/compile_commands.json missing" >&2
  exit 1
fi

# Already running?
if [[ -f "${PID_FILE}" ]] && kill -0 "$(cat "${PID_FILE}")" 2>/dev/null; then
  echo "clangd already running (pid $(cat "${PID_FILE}")). Logs: ${LOG_FILE}"
  exit 0
fi

# Also bail if any clangd has the project compile-commands-dir argument,
# even if our pidfile is stale.
if pgrep -f "clangd.*compile-commands-dir=${PROJECT_ROOT}" >/dev/null; then
  pid=$(pgrep -f "clangd.*compile-commands-dir=${PROJECT_ROOT}" | head -1)
  echo "${pid}" > "${PID_FILE}"
  echo "clangd already running (pid ${pid}, recovered). Logs: ${LOG_FILE}"
  exit 0
fi

# clangd's --background-index thread only fires after a successful LSP
# `initialize` handshake. Without an LSP client clangd sits idle at 0% CPU
# even with --background-index. We launch a tiny Python driver that sends
# initialize + initialized then holds the connection open while the
# background indexer crawls compile_commands.json.
DRIVER="$(dirname "$(readlink -f "$0")")/clangd-driver.py"
if [[ ! -f "${DRIVER}" ]]; then
  echo "error: ${DRIVER} missing" >&2
  exit 1
fi

PROJECT_ROOT="${PROJECT_ROOT}" CLANGD_LOG="${LOG_FILE}" \
  nohup python3 "${DRIVER}" </dev/null >>"${LOG_FILE}" 2>&1 &

DRIVER_PID=$!
sleep 2
CLANGD_PID_FILE_INNER="${PID_FILE}.clangd"
if [[ -f "${CLANGD_PID_FILE_INNER}" ]]; then
  PID=$(cat "${CLANGD_PID_FILE_INNER}")
else
  # Driver hadn't written the pid yet; fall back to driver pid.
  PID="${DRIVER_PID}"
fi
echo "${PID}" > "${PID_FILE}"
echo "${DRIVER_PID}" > "${PID_FILE}.driver"
echo "clangd launched (pid ${PID}). Indexing — log: ${LOG_FILE}"
echo "Expect a long first pass on large projects. Verify with: tail -f ${LOG_FILE}"
