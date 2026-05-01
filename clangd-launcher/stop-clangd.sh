#!/usr/bin/env bash
# Stop the clangd indexer launched by start-clangd.sh.
set -euo pipefail

PID_FILE="${CLANGD_PID_FILE:-/tmp/clangd.pid}"
PROJECT_ROOT="${PROJECT_ROOT:-${UE_ROOT:-}}"

if [[ -f "${PID_FILE}" ]]; then
  pid=$(cat "${PID_FILE}")
  driver_pid=""
  if [[ -f "${PID_FILE}.driver" ]]; then
    driver_pid=$(cat "${PID_FILE}.driver")
  fi
  killed_any=0
  # Kill the driver first; its SIGTERM handler propagates to clangd.
  for p in ${driver_pid} ${pid}; do
    if [[ -n "${p}" ]] && kill -0 "${p}" 2>/dev/null; then
      kill "${p}" 2>/dev/null || true
      killed_any=1
      echo "sent SIGTERM to pid ${p}"
    fi
  done
  rm -f "${PID_FILE}" "${PID_FILE}.driver" "${PID_FILE}.clangd"
  if (( killed_any )); then
    exit 0
  fi
  echo "pidfile stale; nothing alive"
fi

# Fallback: locate by argv
if [[ -n "${PROJECT_ROOT}" ]]; then
  pid=$(pgrep -f "clangd.*compile-commands-dir=${PROJECT_ROOT}" || true)
  if [[ -n "${pid}" ]]; then
    kill ${pid}
    echo "sent SIGTERM to clangd pid ${pid} (recovered via pgrep)"
    exit 0
  fi
fi

echo "no clangd indexer running"
