#!/bin/bash
# extract-verdict.sh — parse a reviewer verdict-file (markdown with YAML
# frontmatter) and emit machine-readable JSON on stdout.
#
# Usage:
#   extract-verdict.sh <verdict-file.md>
#
# Output (success): a JSON object with schema_version, reviewer, branch,
# tip, base, verdict, summary, issues[], followups[], strengths[].
#
# Output (parse failure):
#   {"error": "<reason>", "path": "<input-path>"}
#
# Exit codes:
#   0 = clean parse, all required frontmatter fields present
#   2 = parse failure (file missing, malformed YAML frontmatter, missing
#       required field, or unknown verdict word)
#
# Required frontmatter fields: reviewer, branch, tip, base, verdict, summary.
# Verdict must be one of: PROCEED, PROCEED_WITH_FOLLOWUP, BLOCK, BOUNCE_BACK,
# REJECT. BLOCK and BOUNCE_BACK are accepted aliases. Issue / followup
# bullets match `- [SEV] <text>` with SEV in {HIGH, MED, LOW}; bullets that
# don't match are silently ignored.
#
# No external deps beyond python3. Hand-written YAML mini-parser.
set -euo pipefail

if [ $# -ne 1 ]; then
  printf '{"error":"usage: extract-verdict.sh <verdict-file.md>","path":""}\n' >&2
  exit 2
fi

VERDICT_FILE="$1"

python3 - "$VERDICT_FILE" <<'PY'
import json
import os
import sys

path = sys.argv[1]


def fail(reason: str) -> None:
    print(json.dumps({"error": reason, "path": path}))
    sys.exit(2)


if not os.path.exists(path):
    fail(f"file not found: {path}")
if not os.path.isfile(path):
    fail(f"not a regular file: {path}")

try:
    with open(path, "r", encoding="utf-8") as fh:
        text = fh.read()
except OSError as exc:
    fail(f"read error: {exc}")

# Strip a leading BOM if present, then split lines.
if text.startswith("﻿"):
    text = text[1:]
lines = text.splitlines()

# Skip leading blank lines, find opening `---`.
i = 0
while i < len(lines) and lines[i].strip() == "":
    i += 1
if i >= len(lines) or lines[i].strip() != "---":
    fail("missing opening '---' frontmatter delimiter")

i += 1
fm = {}
fm_start = i
while i < len(lines) and lines[i].strip() != "---":
    raw = lines[i]
    stripped = raw.strip()
    # Skip blank lines inside frontmatter (tolerated).
    if stripped == "":
        i += 1
        continue
    # Comments allowed (YAML convention).
    if stripped.startswith("#"):
        i += 1
        continue
    # Must be `key: value`. Split on first ':'.
    if ":" not in stripped:
        fail(f"frontmatter line {i + 1} not 'key: value': {stripped!r}")
    key, _, value = stripped.partition(":")
    key = key.strip()
    value = value.strip()
    # Strip a single layer of surrounding quotes (single or double).
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        value = value[1:-1]
    if not key:
        fail(f"frontmatter line {i + 1} has empty key")
    fm[key] = value
    i += 1

if i >= len(lines):
    fail("missing closing '---' frontmatter delimiter")
# Step past the closing delimiter.
i += 1

# Schema-version handshake. The first breaking schema change before this
# field existed would have been a silent flag day — old verdict files in
# /tmp/ would have parsed under the new rules with subtly different
# semantics, the merge gate would have made wrong decisions, and nobody
# would have known until the post-mortem. The `schema_version` field
# pins which contract this file expects:
#
#   1 — current schema (5-word verdict vocabulary, fenced-code-block
#       awareness, case-insensitive heading + severity matching, six
#       required frontmatter fields).
#
# Files without `schema_version` are treated as legacy (pre-versioning,
# pre-2026-04-26): downgraded to best-effort parse + a warning emitted
# on stderr. The parser still attempts the full schema but does not
# reject on missing-version. When the schema next changes incompatibly
# (e.g., new required field, removed verdict word) the legacy path
# stays at v1 semantics and a `schema_version: 2` cohort moves
# forward — that is the whole point of the field.
SUPPORTED_SCHEMA_VERSIONS = {"1"}
CURRENT_SCHEMA_VERSION = "1"

raw_schema_version = fm.get("schema_version", "").strip()
if raw_schema_version == "":
    print(
        f"warning: {path}: missing 'schema_version' field; treating as "
        f"legacy (pre-versioning) and parsing best-effort against v1 "
        f"semantics. Add 'schema_version: {CURRENT_SCHEMA_VERSION}' to "
        f"the frontmatter to silence this warning.",
        file=sys.stderr,
    )
    schema_version = CURRENT_SCHEMA_VERSION
elif raw_schema_version not in SUPPORTED_SCHEMA_VERSIONS:
    fail(
        f"unsupported schema_version {raw_schema_version!r}; this parser "
        f"supports {sorted(SUPPORTED_SCHEMA_VERSIONS)}. Upgrade the parser "
        f"or downgrade the verdict file."
    )
else:
    schema_version = raw_schema_version

required = ["reviewer", "branch", "tip", "base", "verdict", "summary"]
missing = [k for k in required if k not in fm or fm[k] == ""]
if missing:
    fail(f"missing required frontmatter field(s): {','.join(missing)}")

# Single source of truth for the canonical verdict-word set. Mirrored in
# .claude/skills/dispatch-review/SKILL.md "Mandatory verdict-file schema"
# and in the per-voice templates' "Verdict format" block. If you change
# this set you MUST update both — the templates instruct reviewers what
# to write; the parser enforces what the merge gate accepts.
#
# Word semantics (see SKILL.md for full text):
#   PROCEED                — clean, merge as-is.
#   PROCEED_WITH_FOLLOWUP  — merge OK, named follow-ups required.
#   BLOCK                  — do not merge until issues addressed; reviewer
#                            does not commit to re-review (parent decides
#                            whether to re-dispatch the same voice or a
#                            different one).
#   BOUNCE_BACK            — do not merge; reviewer expects a follow-up
#                            cycle re-dispatched against this voice.
#   REJECT                 — design wrong; summary must name an
#                            alternative-path recommendation.
VALID_VERDICTS = {
    "PROCEED",
    "PROCEED_WITH_FOLLOWUP",
    "BLOCK",
    "BOUNCE_BACK",
    "REJECT",
}
verdict = fm["verdict"].strip()
if verdict not in VALID_VERDICTS:
    fail(f"invalid verdict {verdict!r}; expected one of {sorted(VALID_VERDICTS)}")

# `lease_compliance` is a v1.5 add-on (2026-04-29) for the
# dispatch-review-lease-compliance branch. The field is OPTIONAL: when a
# reviewer was dispatched without a lease, they omit the field and the
# parser fills `not-applicable`. When a lease was dispatched, the
# reviewer writes `clean` or `out-of-scope`; `out-of-scope` paired with
# `PROCEED` / `PROCEED_WITH_FOLLOWUP` is inconsistent and the parser
# emits a stderr warning (the merge gate is the policy enforcer; the
# parser only normalizes + flags). Legacy files (no field) and explicit
# `not-applicable` collapse to the same JSON output, so consumers can
# rely on the key being present.
VALID_LEASE_COMPLIANCE = {"clean", "out-of-scope", "not-applicable"}
raw_lease = fm.get("lease_compliance", "").strip()
if raw_lease == "":
    lease_compliance = "not-applicable"
elif raw_lease not in VALID_LEASE_COMPLIANCE:
    fail(
        f"invalid lease_compliance {raw_lease!r}; expected one of "
        f"{sorted(VALID_LEASE_COMPLIANCE)}"
    )
else:
    lease_compliance = raw_lease

if lease_compliance == "out-of-scope" and verdict in {"PROCEED", "PROCEED_WITH_FOLLOWUP"}:
    print(
        f"warning: {path}: lease_compliance=out-of-scope is inconsistent "
        f"with verdict={verdict}; the merge gate should treat this as "
        f"BOUNCE_BACK regardless of the verdict word.",
        file=sys.stderr,
    )

# Body parsing: walk sections looking for `## Issues / Concerns`,
# `## Followups`, `## Strengths`. Bullets gated on `- [SEV] <text>`.
#
# Fenced-code-block awareness: reviewers paste source excerpts (```rust,
# ```python, ```text, plain ```) inside `## Issues / Concerns` to anchor
# their findings. Bullets that appear *inside* a fence are excerpts of
# someone else's code, not findings, and must NOT be parsed as issues.
# Track the fence state per-line; toggle on any line whose first
# non-whitespace token is ``` (with optional language tag). Skip section
# heading + bullet logic while inside a fence.
issues = []
followups = []
strengths = []

SECTION_ISSUES = "issues"
SECTION_FOLLOWUPS = "followups"
SECTION_STRENGTHS = "strengths"
SECTION_NONE = None

current = SECTION_NONE
in_fence = False
for raw in lines[i:]:
    stripped = raw.rstrip()
    fence_probe = stripped.lstrip()
    # Fence toggle. CommonMark allows 3+ backticks; we accept any run of
    # 3+ as the opener/closer (the closer doesn't have to match length
    # because we don't validate nested-fence semantics — a single
    # toggle-on / toggle-off model is enough for verdict files).
    if fence_probe.startswith("```"):
        in_fence = not in_fence
        continue
    if in_fence:
        continue
    if stripped.startswith("## "):
        heading = stripped[3:].strip().lower()
        if heading.startswith("issues"):
            current = SECTION_ISSUES
        elif heading.startswith("followups"):
            current = SECTION_FOLLOWUPS
        elif heading.startswith("strengths"):
            current = SECTION_STRENGTHS
        else:
            current = SECTION_NONE
        continue
    if current is None:
        continue
    s = stripped.lstrip()
    if not s.startswith("- "):
        continue
    body = s[2:].lstrip()
    if current == SECTION_STRENGTHS:
        strengths.append(body)
        continue
    # Issues / Followups: must be `[SEV] <text>` with SEV in HIGH/MED/LOW.
    if not body.startswith("["):
        continue
    rb = body.find("]")
    if rb == -1:
        continue
    sev = body[1:rb].strip().upper()
    if sev not in ("HIGH", "MED", "LOW"):
        continue
    text_after = body[rb + 1 :].lstrip()
    entry = {"severity": sev, "text": text_after}
    if current == SECTION_ISSUES:
        issues.append(entry)
    else:
        followups.append(entry)

out = {
    "schema_version": schema_version,
    "reviewer": fm["reviewer"],
    "branch": fm["branch"],
    "tip": fm["tip"],
    "base": fm["base"],
    "verdict": verdict,
    "summary": fm["summary"],
    "lease_compliance": lease_compliance,
    "issues": issues,
    "followups": followups,
    "strengths": strengths,
}
print(json.dumps(out))
PY
