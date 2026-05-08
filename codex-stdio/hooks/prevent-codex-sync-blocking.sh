#!/usr/bin/env bash
# prevent-codex-sync-blocking.sh -- PreToolUse hook that blocks
# synchronous codex MCP shims (`mcp__codex-stdio__codex_run_task`,
# `mcp__codex__codex`) and instructs the parent MCP client to use
# the `Bash` + `run_in_background=true` pattern instead.
#
# ## Why
#
# The codex MCP tools are synchronous JSON-RPC calls: the parent
# blocks until the child returns. Even when the parent batches N
# calls in one response, the longest one walls everyone behind it
# (codex impl packets typically run 3-5 minutes each). That defeats
# the entire point of fanning out parallel codex workers.
#
# The right pattern is `Bash` with `run_in_background=true` invoking
# `codex exec` directly. The parent gets a Bash background ID
# immediately and can keep dispatching; completion is surfaced
# independently per task.
#
# ## Behaviour
#
# - tool_name is `mcp__codex-stdio__codex_run_task` OR
#   `mcp__codex__codex` -> print the bash-bg redirect block to
#   stderr, exit 2 (block).
# - $ALLOW_CODEX_SYNC=1 -> exit 0 (passthrough). Rare escape hatch
#   for genuine sync-required cases (e.g., one-off probe where
#   blocking is desired).
# - any other tool_name -> exit 0 (passthrough).
#
# Fail-OPEN on infra errors (no jq + no python3, malformed JSON,
# empty stdin) -- the parent contract is still authoritative; the
# hook is a belt for the suspenders.
#
# ## Wire-up
#
# Register as a PreToolUse hook in your MCP client's settings
# (e.g. Claude Code .claude/settings.json). The hook receives the
# tool call as JSON on stdin and returns exit 2 to block.

set -uo pipefail

# Override knob: explicit operator opt-in for sync codex calls.
if [ "${ALLOW_CODEX_SYNC:-}" = "1" ]; then
    exit 0
fi

# Read full stdin; tolerate EOF / empty payload.
STDIN_RAW=$(cat 2>/dev/null || true)
[ -z "$STDIN_RAW" ] && exit 0

# Extract .tool_name. Prefer jq when present; fall back to python3.
TOOL_NAME=""
if command -v jq >/dev/null 2>&1; then
    TOOL_NAME=$(printf '%s' "$STDIN_RAW" | jq -r '.tool_name // empty' 2>/dev/null || true)
elif command -v python3 >/dev/null 2>&1; then
    TOOL_NAME=$(printf '%s' "$STDIN_RAW" | python3 -c '
import json, sys
try:
    d = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
v = d.get("tool_name")
if isinstance(v, str):
    sys.stdout.write(v)
' 2>/dev/null || true)
else
    # Neither parser available -> fail-OPEN.
    exit 0
fi

case "$TOOL_NAME" in
    mcp__codex-stdio__codex_run_task|mcp__codex__codex)
        ;;
    *)
        exit 0
        ;;
esac

cat >&2 <<'MSGEOF'
[prevent-codex-sync-blocking] BLOCKED: mcp__codex-* tools synchronously block the parent.

For non-blocking parallel dispatch, use Bash with run_in_background=true:

  Bash(
    command="cd <worktree> && \
             codex exec --sandbox=danger-full-access --skip-git-repo-check \
               --model=<model> \
               --output-last-message=/tmp/codex-<branch>-msg.txt \
               < /tmp/codex-<branch>-prompt.txt \
               > /tmp/codex-<branch>.log 2>&1",
    run_in_background=true,
    description="codex on <branch>"
  )

Returns a Bash background ID immediately; the parent keeps dispatching.
For multi-packet waves, fire N parallel Bash run_in_background calls in
ONE response. The harness's notification system surfaces each completion
independently.

Override this hook (rare) with: ALLOW_CODEX_SYNC=1
MSGEOF
exit 2
