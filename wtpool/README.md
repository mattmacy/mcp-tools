# wtpool

Read-only stdio MCP server + CLI exposing per-worktree git state and
in-flight CLI-agent telemetry. Replaces several Bash calls every
cascade-merge planning round tends to pay (`git worktree list`,
`git -C <wt> log main..HEAD --oneline`, `git -C <wt> status
--porcelain`, optional `git -C <wt> rev-parse HEAD`, `ls /tmp/agent-*`
cross-check) with cached MCP tools.

## Tool surface

| Tool | Input | Output |
|---|---|---|
| `worktree_list` | `{}` | `{worktrees: [{path, branch, tip_sha, commits_ahead, dirty, is_main}]}` |
| `worktree_state` | `{path: <abs>}` | `{branch, tip_sha, commits_ahead, files_changed, last_log_lines, untracked_count, dirty}` |
| `agent_inflight_summary` | `{stale_minutes?: int}` (default 5) | `{stale_minutes, worktrees: [{path, branch, last_agent_tool_ts, agent_ids, stale}]}` |
| `pending_review` | `{branch: string}` | `{torvalds: {…} \| null, lattner: {…} \| null}` |
| `merge_to_main` | `{branch, reviewer_voices, merge_message_subject, merge_message_body, …}` | merge result; refuses self-merge per reviewer-voice policy |
| `pool_acquire` | `{branch_name?, base_sha?}` | `{path, base_sha, branch?}` — claim a free slot, drive HEAD to `base_sha` (default current main tip), optionally create `branch_name` via `git checkout -b`. Atomic with the rebase step (eliminates stale-base BOUNCE_BACK). |
| `pool_release` | `{path, force?}` | `{released, path}` — detach + reset + clean; refuses unmerged-ahead-of-main without `force` |
| `pool_status` | `{}` | `{free: [{path, head_sha}], in_use: [{path, branch, commits_ahead}]}` |
| `worktree_lease_get` | `{worktree_path}` | parsed lease JSON; errors with `error_kind` ∈ {`io`,`invalid_json`,`missing_field`,`unsupported_schema_version`,`invalid_task_id`,`invalid_timestamp`} |
| `worktree_lease_emit` | `{worktree_path, task_id, worker, branch, allowed_paths?, forbidden_paths?, test_commands?, merge_authority?, expires_at?, parent_task_id?}` | `{wrote, task_id, schema_version}` — atomic write to `<worktree>/.wt-lease.json` |
| `worktree_lease_check` | `{worktree_path, target_path}` | `{target, allowed: bool}` — forbidden globs take precedence over allowed |

All four tools share a 60-second TTL `Mutex<HashMap>` cache. TTL-only
(no inotify) — a freshly-created worktree may take up to one minute to
appear in `worktree_list`.

`verdict_word` extraction. The spec says "first whitespace token of
file's first line, lowercased." Reviewer agents canonically open with
`VERDICT: <PROCEED|BOUNCE_BACK|REVERT|REJECT|...>`, which makes the
literal first token always `VERDICT:` and useless. We normalise: when
the first token is `verdict[:]`, return the SECOND token instead;
otherwise return the first. Either way the result is lowercased and
trailing colon stripped.

## CLI

```
wtpool worktree-list
wtpool worktree-state /tmp/wtpool/<name>
wtpool agent-inflight [--stale-minutes 5]
wtpool pending-review <branch>
wtpool pool-status
wtpool pool-acquire --branch-name <branch> [--base-sha <sha>]
wtpool pool-release --path <abs> [--force]
wtpool probe        # smoke test (see below)
wtpool serve-mcp    # JSON-RPC 2.0 stdio MCP server
```

## Pool RPCs (M5 — pool-first dispatch enforcement)

`pool_acquire` / `pool_release` / `pool_status` mechanise the
worktree-pool dispatch reflex. Parent dispatcher previously had to
manually:

1. call `worktree_list`, filter for `branch == "<detached>"` + `dirty == false`,
2. pick a path from the filtered set,
3. inline a `git -C <slot> checkout -b <branch> <main-tip>` step into
   the dispatch prompt.

`pool_acquire` collapses 1-3 into one RPC. **Atomic-acquire (P0
2026-04-28).** The slot's branch is always created at *current* main
tip when `base_sha` is omitted — never at the slot's stale detached
HEAD (which can lag main by hours/days/weeks if the slot has been
idle). Pre-existing detached HEAD ≠ implicit base; the explicit start
point on `git checkout -b <branch_name> <resolved_base>` repoints the
working tree forward to the resolved base. `branch_name` is optional:
omit it to receive a slot still detached at `base_sha` for callers
that want to do non-standard branch operations themselves. Race-
safety: each slot has a per-acquire lockfile at `/tmp/wtpool-
acquire-<hash(canon-path)>.lock` opened with `O_EXCL`
(`OpenOptions::create_new`). Two concurrent acquires racing for the
same slot have at most one winner; the loser falls through to the
next free slot. If all slots lose the lockfile race the call returns
the same `pool exhausted` error a steady-state-exhausted call would.

`pool_release` reverses the acquire but refuses to drop a slot whose
branch has commits ahead of main not yet merged in (would silently
lose work). Pass `force=true` to override.

`--repo <path>` overrides the repo root (default
`$WTPOOL_REPO` then `<REPO_ROOT>`).

## Probe expected output

`wtpool probe` exercises three of the four tools against
the configured repo and emits a single JSON object on stdout. Keys:

```json
{
  "worktree_list": { "worktrees": [...] },
  "agent_inflight_summary": { "stale_minutes": 5, "worktrees": [...] },
  "pending_review_smoke": { "torvalds": { "exists": false, ... }, "lattner": { "exists": false, ... } }
}
```

The probe pings `pending_review` with the synthetic branch
`_probe_branch_` so the smoke test does not depend on an actual
verdict file existing. Exit code 0 on full success, 1 if any of the
three tools reported a tool-failure (each error captured per-key
under `"error"`).

`worktree_state` is intentionally NOT exercised — it requires a path
argument, and the probe is meant to run against any repo without
caller-supplied arguments. Use `worktree-state` directly for the
single-worktree path.

## merge_to_main guardrails

Two upfront checks gate every `merge_to_main` invocation; both prevent
the failure mode that landed bad merge `107974d4` on 2026-04-29 (oracle
branch content shipped under animation-branch subject).

**Branch ↔ worktree consistency.** `merge_to_main` reads
`git -C <worktree_path> rev-parse --abbrev-ref HEAD` and refuses when
the result does not equal `req.branch`. Eliminates the
"rebase-wrong-branch-in-wrong-worktree" silent-success path: the rebase
step is a no-op when wrong worktree branch is already up to date with
main, but `git -C <main> merge --no-ff <branch>` still resolves the
correct branch by name and lands its content under whatever subject
the caller authored. Surfacing this mismatch upfront keeps the merge
commit's body honest. No bypass — fix the dispatch instead.

**Subject must contain branch name.** `validate_request` refuses when
`req.merge_message_subject` does not contain `req.branch` as a
case-sensitive substring. Rule 10 enforcement: the subject is the
human-visible identity of the merge commit; if it names a different
branch than what is being merged, downstream review of the commit
becomes a search exercise. Set `BYPASS_SUBJECT_BRANCH_CHECK=1` in the
environment for the rare deliberate-rename case (e.g. a research
branch landing under its renamed final shape — must be intentional,
not accidental). Only the literal value `1` activates the bypass;
`true` / `yes` / `0` / empty do not.

Both guardrails live in `tools/wtpool/src/merge.rs`
(`validate_request` for subject/branch, `merge_to_main` orchestrator
for branch/worktree). Mutation probes per CLAUDE.md Rule 11:
`validate_rejects_subject_without_branch_name`,
`validate_bypass_only_accepts_literal_one`,
`rejects_when_worktree_head_branch_does_not_match_req_branch`.

## Path-validation guardrails

`worktree_state` rejects:

- Relative paths (must be absolute).
- Paths outside `<REPO_ROOT>` (or its canonicalised form).

`pending_review` rejects:

- Empty branch names.
- Names containing characters outside `[A-Za-z0-9_./-]`.
- Names containing `..` or beginning with `-`.

These guards live at `tools/wtpool/src/git.rs::validate_worktree_path`
and `tools/wtpool/src/reviews.rs::pending_review` respectively.

## Tests

```
cargo test -p wtpool
```

103 unit tests + 4 integration tests + 8 merge-dry-run tests + 4
merge-to-main end-to-end smoke tests across `cache`, `git`, `agents`,
`reviews`, `merge`, `pool`, and `mcp` modules. Integration tests
under `tests/integration.rs` exercise the full `serve()` MCP loop
end-to-end plus the public lib surface against a `git2`-built temp
repo. `pool::tests` includes a race-safety thread-pair test that
proves two concurrent acquires land on distinct slots.
`tests/merge_to_main_smoke.rs` runs the full Spec §3.3 6-step
orchestration against a `git2`-built fixture: clean rebase + real
merge commit (asserts `--no-ff` honored via `parent_count == 2`
mutation probe per CLAUDE.md Rule 11), rebase-conflict abort with
main untouched + rebase state cleaned up, and cumulative-doc auto-
resolve path lands the merge with `cumulative_md_resolved=true`.

## Integration

Register in your MCP client's config (e.g. `.mcp.json`) as
`/usr/local/bin/wtpool serve-mcp`.

Build + install:

```sh
cargo build --release -p wtpool
sudo install -m 755 target/release/wtpool /usr/local/bin/
```

## Subagent template

`wtpool/agents/worktree-worker.md` is a **Claude Code subagent
template** that codifies the dispatch contract this server enforces:
pool-first acquire, branch ↔ worktree consistency, no bash
`git merge --no-ff` (use `merge_to_main` RPC instead), and the
ban-list of dangerous git ops. Fill in the bracketed placeholders
(`<WORKTREE_ROOT>`, `<wtpool-server>`, test/format commands) and
register under your project's `.claude/agents/`.

The template assumes foreground-only dispatch; a 2026-04-17 probe in
the originating project found that background subagents are denied
basic git invocations regardless of `permissionMode: bypassPermissions`.
Retest before re-enabling background dispatch on a different harness.

## Git library backend

`git2 ~= 0.20` with `vendored-libgit2` feature so the install does
not require system `libgit2-dev`. CI / dev container both lack a
system libgit2; the vendored build adds ~5 s to first compile but
keeps the image footprint stable. Tilde-pinned to the same minor
range the rest of the in-tree shims use.

## Worktree Lease

The `worktree_lease_*` tools and the `lease` CLI subcommand emit and
consume a per-worktree JSON contract that constrains a worker to a
scoped set of paths and test commands. The lease lives at
`<worktree>/.wt-lease.json`. Schema source:
`wtpool/schemas/worktree-lease.v1.json`.

This crate ships only the schema + readers + writers + validators;
enforcement (e.g. pre-exec / post-exec hooks rejecting off-lease
writes and unauthorized bash commands) is the responsibility of
the dispatching harness.

### Schema fields (v1)

| Field | Required | Notes |
|---|---|---|
| `schema_version` | yes | Must be `1` for this binary to accept the lease. |
| `task_id` | yes | Stable identifier matching `^[A-Za-z0-9][A-Za-z0-9._-]*$`. |
| `worker` | yes | One of `claude-opus`, `claude-sonnet`, `claude-haiku`, `codex`, `human`. |
| `worktree` | yes | Absolute filesystem path. Writer validates it exists. |
| `branch` | yes | Git branch the worker commits on. |
| `allowed_paths` | yes | Array of glob patterns. Empty array means "no writes allowed." |
| `forbidden_paths` | yes | Array of glob patterns. Wins over `allowed_paths` on overlap. |
| `test_commands` | yes | Array of exact-match shell command strings. No prefix or substring match. |
| `merge_authority` | yes | Reviewer voice. Defaults to `review-agent`. |
| `created_at` | yes | RFC 3339 / ISO 8601 timestamp. |
| `expires_at` | no | Optional soft-expiry RFC 3339 timestamp. |
| `parent_task_id` | no | Optional nested-dispatch correlation; reserved field. |

### Glob semantics

Patterns support `*` (single-segment wildcard, does not cross `/`) and
`**` (cross-segment wildcard). All other characters match literally.
Question marks, character classes, and brace expansion are NOT
supported — security-via-simplicity. See module docs in
`src/lease.rs` for the full grammar.

When a path matches both `allowed_paths` and `forbidden_paths`,
`forbidden_paths` wins. This makes
`allowed_paths: ["src/**"]` + `forbidden_paths: ["src/orchestration/**"]`
safe to write without overlap analysis.

### Test command match

`test_commands` is matched **exactly**. A prefix or substring rule
would let `cargo test -p X --release; rm -rf /` past the gate; the
exact-match rule makes the array the literal whitelist of bash
invocations the sister-branch hook permits.

### CLI

```
wtpool lease emit \
    --worktree /tmp/wtpool/wt-04 \
    --task-id  physics-bridge-017 \
    --worker   codex \
    --branch   physics-bridge-017 \
    --allowed  'src/physics/**' \
    --allowed  'tests/physics/**' \
    --forbidden 'CLAUDE.md' \
    --forbidden '.claude/**' \
    --test     'cargo test -p mycrate-physics'

wtpool lease validate --worktree <path>
wtpool lease check    --worktree <path> --target src/physics/foo.rs
```

`lease validate` re-reads + re-validates an existing lease (catches
post-emit corruption or hand-edits). `lease check` answers the
`worktree_lease_check` MCP question from the shell — useful for hook
debugging.

### Sample lease

```json
{
  "schema_version": 1,
  "task_id": "physics-bridge-017",
  "worker": "codex",
  "worktree": "/tmp/wtpool/wt-04",
  "branch": "physics-bridge-017",
  "allowed_paths": ["src/physics/**", "tests/physics/**"],
  "forbidden_paths": ["CLAUDE.md", ".claude/**", "tools/guardian/**"],
  "test_commands": ["cargo test -p mycrate-physics"],
  "merge_authority": "review-agent",
  "created_at": "2026-04-29T15:00:00Z"
}
```
