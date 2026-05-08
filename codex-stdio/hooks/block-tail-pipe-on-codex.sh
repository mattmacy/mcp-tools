#!/bin/bash
# PreToolUse:Bash — block `codex exec ... | tail [...]` shapes.
#
# Why: `codex exec ... 2>&1 | tail -N` buffers all output until the
# codex process exits. If codex iterates for 30+ minutes, the parent
# MCP client sees ZERO output until end-of-run; can't tell hung from
# working. Use `codex ... > /tmp/log 2>&1 &` (background, real-time
# log) or `codex ... 2>&1 | tee /tmp/log` (real-time mirror) instead.
#
# Override: prefix the bash command with TAIL_PIPE_OK=1 (rare).
#
# Wire-up: register as a PreToolUse hook for the Bash tool in your MCP
# client's settings (e.g. Claude Code .claude/settings.json).
set -euo pipefail

INPUT=$(cat)

CMD=$(printf '%s' "$INPUT" | python3 -c '
import json, sys
try:
    d = json.loads(sys.stdin.read())
    print(d.get("tool_input", {}).get("command", ""))
except Exception:
    sys.exit(1)
' 2>/dev/null) || exit 0

# Fast-path: not a codex command.
if ! printf '%s' "$CMD" | grep -qE 'codex[[:space:]]+exec'; then
    exit 0
fi

# Override.
if printf '%s' "$CMD" | grep -qE '^[[:space:]]*TAIL_PIPE_OK=1[[:space:]]'; then
    exit 0
fi

# Detect `codex exec ... | tail [...]` shape. Conservative regex:
# anything matching `codex exec` followed (eventually) by `| tail`.
# Don't false-positive on `tail` as a non-pipe word elsewhere
# (e.g. file path "/tmp/tail-foo.log").
if printf '%s' "$CMD" | grep -qE 'codex[[:space:]]+exec.*\|[[:space:]]*tail([[:space:]]|$)'; then
    cat >&2 <<'EOF'
[block-tail-pipe-on-codex] BLOCKED: codex exec piped through tail.
[block-tail-pipe-on-codex]
[block-tail-pipe-on-codex] tail buffers output until codex exits. If codex iterates
[block-tail-pipe-on-codex] for 30+ minutes, the parent MCP client sees ZERO output
[block-tail-pipe-on-codex] until end — can't distinguish hung from working.
[block-tail-pipe-on-codex]
[block-tail-pipe-on-codex] Use one of:
[block-tail-pipe-on-codex]   codex exec ... > /tmp/log 2>&1 &              (bg, real-time log)
[block-tail-pipe-on-codex]   codex exec ... 2>&1 | tee /tmp/log            (real-time mirror)
[block-tail-pipe-on-codex]
[block-tail-pipe-on-codex] Override (rare): TAIL_PIPE_OK=1 prefix.
EOF
    exit 2
fi

exit 0
