//! `merge_to_main` orchestration.
//!
//! Six steps:
//!
//! 1. Snapshot pre_state (`main_tip`, `branch_tip`).
//! 2. `git -C <worktree> rebase main`. Conflict outside the
//!    cumulative-doc → `git rebase --abort`, return `rebase_conflict`.
//!    NEVER auto-resolve outside the cumulative doc.
//! 3. Cumulative-doc-only conflict + `auto_resolve_cumulative_md=true`
//!    → invoke [`crate::cumulative_md::resolve_cumulative_md_conflict`]
//!    on the file body, `git add` it, `git rebase --continue`. If
//!    the heuristic bails: abort rebase, return `rebase_conflict`
//!    with `conflict_kind: "content_mixed"`.
//! 4. Compose `Reviewed-by:` trailer from `reviewer_voices`. Reject
//!    if the list is empty or contains only `"worktree-worker"`,
//!    unless the branch falls under the carveout allowlist
//!    ([`RULE13_CARVEOUT_ALLOWLIST`]).
//! 5. Write merge message to `/tmp/merge-msg-<branch>-<sha8>.txt`,
//!    run `ALLOW_MAIN_COMMIT=1 git -C /repo merge --no-ff
//!    <branch> -F <tmp>` via [`crate::git_exec`].
//! 6. Verify post-state main_tip differs + new tip's commit message
//!    contains the trailer line. Failure → return error with
//!    `hook_output`.
//!
//! `dry_run=true`: stop after step 4. Write proposed message to
//! `/tmp/merge-msg-<branch>-DRYRUN.txt`, return
//! `proposed_message_path` plus the would-be conflict list (always
//! empty when called dry-run since steps 2/3 don't run).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::cumulative_md::{resolve_cumulative_md_conflict, ConflictKind};
use crate::git_exec::{git, git_with_main_commit_override, render_command};
use crate::lease::glob_match;

/// Canonical relative-path of the cumulative-doc inside the repo —
/// the only file `auto_resolve_cumulative_md` will touch.
pub(crate) const CUMULATIVE_MD_REL: &str = "docs/plans/cumulative.md";

/// Carveout allowlist — glob patterns matched against repo-relative
/// paths in `git diff main...HEAD --name-only`. Branches that touch
/// ONLY allowlisted paths may merge with empty `reviewer_voices`
/// (the carveout path).
///
/// Blast-radius reasoning: every entry below is documentation,
/// research notes, or harness data — none can break a build, regress
/// a runtime invariant, or alter a hot path. Any change to the list
/// requires a fresh blast-radius justification per entry.
///
/// Glob semantics mirror [`crate::lease::glob_match`]: `*` is a
/// single-segment wildcard, `**` crosses segments.
pub(crate) const RULE13_CARVEOUT_ALLOWLIST: &[&str] = &[
    // Bench fixtures + harness data — no runtime invariant.
    "benchmark/**",
    // Cross-session shared planning + ledger state. Per CLAUDE.md
    // Rule 13, only the agent-ledger / followup-tracker-* /
    // agent-ledger-archive-* files under project/shared/ qualify;
    // every other project/shared/*.md path is reviewed by lattner
    // at minimum. `docs/research/**` and `docs/plans/**` are
    // EXPLICITLY NOT in the carveout (research findings have
    // downstream behavioral consequences; plans drive subsequent
    // dispatch waves), so both require single-voice review.
    "project/shared/agent-ledger.md",
    "project/shared/followup-tracker-*.md",
    "project/shared/agent-ledger-archive-*.md",
    // Routing-policy + manual session-start checklist. Both are
    // pure prose / parameter docs read by parent CC at session
    // start; neither participates in any compile or test.
    "tools/routing-policy.md",
    "STARTUP.md",
];

/// Trailer literal emitted in place of `Reviewed-by: <voice>` when the
/// carveout path is taken.
pub(crate) const RULE13_CARVEOUT_TRAILER: &str = "Reviewed-by: rule-13-carveout";

/// MCP-side result enum for `merge_to_main`. Stable wire strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    /// All six steps succeeded (or step 6 verify mismatched but step
    /// 5's commit landed — see `idempotency_warning`).
    Merged,
    /// Rebase produced a conflict the tool refused to auto-resolve.
    /// Rebase was aborted; main is untouched.
    RebaseConflict,
    /// Pre-merge test gate (`pre-merge-test hook` or equivalent)
    /// failed. Reserved for future wiring; not produced by this
    /// implementation today.
    TestGateFailed,
    /// Hook (`main-commit-policy hook`,
    /// `main-edit protection hook`, …) blocked the merge subprocess.
    HookBlocked,
    /// Pre-state already showed the branch merged into main; nothing
    /// to do.
    AlreadyMerged,
    /// `dry_run=true` short-circuit. No git state was changed.
    DryRun,
}

impl MergeStatus {
    /// Stable wire string per spec output schema.
    pub fn as_wire(&self) -> &'static str {
        match self {
            MergeStatus::Merged => "merged",
            MergeStatus::RebaseConflict => "rebase_conflict",
            MergeStatus::TestGateFailed => "test_gate_failed",
            MergeStatus::HookBlocked => "hook_blocked",
            MergeStatus::AlreadyMerged => "already_merged",
            MergeStatus::DryRun => "dry_run",
        }
    }
}

/// Tool input. Mirrors spec.
#[derive(Debug, Clone)]
pub struct MergeRequest {
    /// Branch name being merged into `main`.
    pub branch: String,
    /// Reviewer voices for the `Reviewed-by:` trailer. reviewer-voice policy: must
    /// be non-empty + must contain at least one voice that is not
    /// `"worktree-worker"`.
    pub reviewer_voices: Vec<String>,
    /// Subject line for the merge commit. Spec §3.2 caps at 72 chars.
    pub merge_message_subject: String,
    /// Body of the merge commit message (after the subject + blank
    /// line). Trailer is appended by the tool.
    pub merge_message_body: String,
    /// Default true. Cumulative-md conflicts auto-resolved via
    /// [`crate::cumulative_md`]. Other conflicts always bail.
    pub auto_resolve_cumulative_md: bool,
    /// Default false. When true, stop after step 4 + return the
    /// proposed message path.
    pub dry_run: bool,
}

/// Validate the inputs reviewer-voice policy require. Pure function
/// over [`MergeRequest`] so tests can exercise it without spawning
/// any git subprocess.
///
/// Guardrail check (added 2026-04-29 after `107974d4` landed under a
/// mismatched subject): the subject must contain `branch` as a
/// case-sensitive substring. Set `BYPASS_SUBJECT_BRANCH_CHECK=1` in
/// the environment to override for the rare deliberate-rename case
/// (e.g. a research/scope branch merging under a renamed final
/// shape). The bypass must be explicit; accidental subject/branch
/// mismatch is the failure mode this guards against.
pub fn validate_request(req: &MergeRequest) -> Result<(), String> {
    if req.branch.trim().is_empty() {
        return Err("merge_to_main: `branch` must be non-empty".into());
    }
    if req.merge_message_subject.len() > 72 {
        return Err(format!(
            "merge_to_main: `merge_message_subject` is {} bytes (UTF-8), max 72",
            req.merge_message_subject.len()
        ));
    }
    if req.merge_message_subject.trim().is_empty() {
        return Err("merge_to_main: `merge_message_subject` must be non-empty".into());
    }
    // Subject ↔ branch substring guardrail. Rule 10: subject must
    // describe what is being merged; mismatched subject lets a wrong-
    // branch merge (rebase-in-wrong-worktree pattern that produced
    // `107974d4`) ship under a misleading description. Allow explicit
    // env bypass for the rare deliberate-rename case.
    let bypass_subject_check =
        std::env::var("BYPASS_SUBJECT_BRANCH_CHECK").ok().as_deref() == Some("1");
    if !bypass_subject_check && !req.merge_message_subject.contains(&req.branch) {
        return Err(format!(
            "merge_to_main: `merge_message_subject` ({:?}) must contain `branch` ({:?}) as substring \
             (Rule 10 — subject must describe what is merged). Set BYPASS_SUBJECT_BRANCH_CHECK=1 to override.",
            req.merge_message_subject, req.branch
        ));
    }
    // Voice-shape happy path lives in [`validate_voices`] — that fn
    // runs AFTER carveout eligibility is computed and handles both
    // empty-voices (carveout) and named-voice cases. But the
    // worktree-worker-only case has NO carveout escape (reviewer-voice policy's
    // self-merge prohibition trumps the blast-radius optimisation), so
    // we reject it here, BEFORE the carveout probe spawns
    // `git diff main...HEAD`. The duplication is intentional: it skips
    // a guaranteed-reject git subprocess on a malformed request, and
    // means a malformed-request response surfaces even on inputs whose
    // worktree probe would itself fail (unrelated git error masking
    // the actual Rule-13 violation). validate_voices repeats the
    // check as defense-in-depth — if a future caller bypasses
    // validate_request, the gate still holds.
    let real_voices_count = req
        .reviewer_voices
        .iter()
        .filter(|v| v.trim() != "worktree-worker" && !v.trim().is_empty())
        .count();
    let only_worktree_worker = !req.reviewer_voices.is_empty() && real_voices_count == 0;
    if only_worktree_worker {
        return Err(
            "merge_to_main: `reviewer_voices` contains only `worktree-worker` (reviewer-voice policy self-merge prohibited; carveout requires empty list, not worker self-attestation)"
                .into(),
        );
    }
    Ok(())
}

/// reviewer-voice policy voice gate — runs AFTER carveout eligibility is computed.
///
/// Two legal shapes:
///
/// - `reviewer_voices` non-empty AND contains at least one
///   non-`worktree-worker` voice → standard review path. Carveout state
///   irrelevant (an explicit reviewer dispatch is always legal).
/// - `reviewer_voices` empty AND `carveout_eligible == true` → carveout
///   path. Trailer becomes `Reviewed-by: rule-13-carveout`.
///
/// Every other shape is rejected. Empty-voices on a non-carveout branch
/// is the load-bearing rejection — it catches the "tried to skip
/// review" case the rule guards against.
pub(crate) fn validate_voices(req: &MergeRequest, carveout_eligible: bool) -> Result<(), String> {
    let real_voices: Vec<&String> = req
        .reviewer_voices
        .iter()
        .filter(|v| v.trim() != "worktree-worker" && !v.trim().is_empty())
        .collect();
    if !real_voices.is_empty() {
        return Ok(());
    }
    // Empty (or worktree-worker-only) voices: carveout is the only
    // legal escape.
    let only_worktree_worker = !req.reviewer_voices.is_empty()
        && req
            .reviewer_voices
            .iter()
            .all(|v| v.trim() == "worktree-worker" || v.trim().is_empty());
    if only_worktree_worker {
        // worktree-worker self-merge is never a carveout candidate —
        // the rule's spirit (no author=reviewer=merger) trumps the
        // blast-radius optimisation.
        return Err(
            "merge_to_main: `reviewer_voices` contains only `worktree-worker` (reviewer-voice policy self-merge prohibited; carveout requires empty list, not worker self-attestation)"
                .into(),
        );
    }
    if carveout_eligible {
        return Ok(());
    }
    Err(
        "merge_to_main: `reviewer_voices` must contain at least one non-`worktree-worker` voice (reviewer-voice policy). Empty list permitted only when every touched path matches the carveout allowlist."
            .into(),
    )
}

/// Pure carveout eligibility check over a list of repo-relative path
/// strings. Returns true iff every path matches at least one
/// [`RULE13_CARVEOUT_ALLOWLIST`] pattern AND the input is non-empty.
///
/// Empty input → false: a no-op merge is not a carveout candidate
/// (would be caught earlier as `AlreadyMerged` anyway, but be explicit).
///
/// Symlink resolution is the caller's responsibility — see
/// [`branch_carveout_eligible`] for the full filesystem-aware check that
/// also rejects symlinks escaping the worktree root.
pub(crate) fn is_carveout_eligible(paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    paths.iter().all(|p| {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            return false;
        }
        RULE13_CARVEOUT_ALLOWLIST
            .iter()
            .any(|pat| glob_match(pat, trimmed))
    })
}

/// Filesystem-aware carveout check: enumerates `git diff main...HEAD
/// --name-only` for the given worktree, applies symlink-escape rejection
/// per touched-existing-file, then defers to [`is_carveout_eligible`]
/// for the allowlist match.
///
/// Symlink rule (hardened post-review): a path is
/// rejected (returns Ok(false)) when EITHER:
///
/// - The path canonicalizes to a target outside the worktree root.
///   Catches `benchmark/symlink-to-core → /external/path/core/src/lib.rs`.
/// - The path IS a symlink (per `fs::symlink_metadata`) AND
///   `fs::canonicalize` errors. Catches the
///   `benchmark/symlink-evil → /tmp/payload-not-yet-created` evasion
///   where the target is planted post-merge so name-only-allowlisting
///   would let it through.
///
/// Real deletions and rename-froms (paths whose `symlink_metadata`
/// errors with NotFound — no symlink, no regular file) are name-only-
/// checked: there's no live target to follow, so the allowlist match
/// against the path string is the load-bearing check.
pub(crate) fn branch_carveout_eligible(worktree: &Path) -> Result<bool, String> {
    let paths = branch_changed_paths(worktree)?;
    if paths.is_empty() {
        return Ok(false);
    }
    // Canonicalize the worktree root once — symlink-escape check
    // compares against this prefix.
    let wt_canon = fs::canonicalize(worktree).map_err(|e| {
        format!(
            "merge_to_main: canonicalize worktree {}: {e}",
            worktree.display()
        )
    })?;
    for p in &paths {
        let abs = worktree.join(p);
        match fs::canonicalize(&abs) {
            Ok(canon) => {
                if !canon.starts_with(&wt_canon) {
                    // Symlink escapes worktree → not eligible.
                    return Ok(false);
                }
            }
            Err(_) => {
                // canonicalize errored. Distinguish two sub-cases via
                // symlink_metadata:
                //
                // - symlink_metadata Ok + is_symlink() → dangling
                //   symlink (target absent or not yet planted). The
                //   defense-in-depth posture: refuse the carveout.
                //   Otherwise an attacker could plant a symlink to
                //   /tmp/payload that does not exist at carveout-check
                //   time and have the build follow it post-merge.
                // - symlink_metadata Err (typically NotFound) → no
                //   filesystem entry at all. This is the legitimate
                //   deletion / rename-from case. Name-only allowlist
                //   match remains; fall through.
                if let Ok(meta) = fs::symlink_metadata(&abs) {
                    if meta.file_type().is_symlink() {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(is_carveout_eligible(&paths))
}

fn branch_changed_paths(worktree: &Path) -> Result<Vec<String>, String> {
    let out = git(worktree, &["diff", "main...HEAD", "--name-only"])?;
    if !out.success() {
        return Err(format!(
            "git diff main...HEAD --name-only failed in {}: {}",
            worktree.display(),
            out.stderr.trim()
        ));
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Canonical reviewer voices accepted by the
/// `reviewed-trailer hook` hook's `CANONICAL_REVIEWERS` allowlist.
/// Kept in sync by hand — when the hook adds a voice, mirror it here.
/// Used only by [`compose_reviewer_trailer`] for the pre-merge warn-log
/// described below; rejection is still the hook's job (load-bearing
/// authority lives in one place).
pub(crate) const CANONICAL_REVIEWER_VOICES: &[&str] = &[
    "torvalds",
    "lattner",
    "carmack",
    "three-panel",
    "human",
    "mechanical",
    "docs-only",
    "worktree-worker",
];

/// Compose the trailer block: one `Reviewed-by: <voice>` line per
/// voice. The hook trailer regex (`_parse_git.py::TRAILER_RE`) matches
/// the first identifier per line, so `+`-joined trailers lose every
/// voice past the first. Voices are emitted verbatim — the
/// `CANONICAL_REVIEWERS` allowlist in `reviewed-trailer hook`
/// rejects `-agent`-suffixed and other non-canonical names, so the
/// caller is responsible for passing canonical voices (see
/// [`CANONICAL_REVIEWER_VOICES`]). Empty input is rejected by
/// [`validate_request`] before this is reached.
///
/// Pre-merge warn-log: any voice not in [`CANONICAL_REVIEWER_VOICES`]
/// emits a `wtpool: non-canonical reviewer voice …` line on
/// stderr so the caller catches typos (e.g. `lattner-agent`) before
/// the hook rejects them mid-merge — saves one round-trip. The trailer
/// is still composed verbatim; this is advisory only.
pub(crate) fn compose_reviewer_trailer(voices: &[String]) -> String {
    if voices.is_empty() {
        // Caller is on the carveout path — emit the literal carveout
        // trailer. compose_merge_message is the sole legitimate caller
        // that may pass an empty list (validated upstream by
        // validate_voices + is_carveout_eligible).
        return RULE13_CARVEOUT_TRAILER.to_string();
    }
    for v in voices {
        let trimmed = v.trim();
        if !trimmed.is_empty() && !CANONICAL_REVIEWER_VOICES.contains(&trimmed) {
            eprintln!(
                "wtpool: non-canonical reviewer voice {trimmed:?} — \
                 hook will reject. Allowed: {CANONICAL_REVIEWER_VOICES:?}"
            );
        }
    }
    voices
        .iter()
        .map(|v| format!("Reviewed-by: {}", v.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compose the full merge-commit message: subject + blank + body +
/// blank + trailer line. Body may be multi-paragraph; trailer is
/// always last. Empty `reviewer_voices` triggers the reviewer-voice policy carveout
/// trailer (`Reviewed-by: rule-13-carveout`); upstream validation in
/// [`validate_voices`] guarantees this is reached only when the branch
/// passes [`is_carveout_eligible`].
pub(crate) fn compose_merge_message(req: &MergeRequest) -> String {
    let trailer = compose_reviewer_trailer(&req.reviewer_voices);
    let mut s = String::new();
    s.push_str(req.merge_message_subject.trim_end());
    s.push('\n');
    s.push('\n');
    let body = req.merge_message_body.trim_end();
    if !body.is_empty() {
        s.push_str(body);
        s.push('\n');
        s.push('\n');
    }
    s.push_str(&trailer);
    s.push('\n');
    s
}

/// Top-level orchestration: returns the JSON payload spec
/// describes. `worktree_path` is the linked worktree the branch is
/// checked out in (where the rebase happens); `main_repo` is the
/// canonical workspace root (where the merge commit lands).
pub fn merge_to_main(
    main_repo: &Path,
    worktree_path: &Path,
    req: &MergeRequest,
) -> Result<Value, String> {
    validate_request(req)?;

    // Branch ↔ worktree consistency guardrail (added 2026-04-29 after
    // `107974d4`). The bad-merge failure mode: parent passed
    // `branch=startup-oracle-all-patches` + `worktree_path=<wt-06>`
    // (which had `animation-phase3-bp-fn-transition-rule` checked out).
    // Rebase ran in wt-06 against the wrong branch (no-op since up-to-
    // date), then `git merge --no-ff <oracle-branch>` on main resolved
    // by name and landed oracle content under animation's subject.
    // Reject upfront when the worktree's HEAD branch does not match
    // `req.branch` — refuses to rebase + merge wrong target.
    let worktree_branch = current_branch(worktree_path)?;
    if worktree_branch != req.branch {
        return Err(format!(
            "merge_to_main: worktree_path {:?} has branch {:?} checked out but `branch` param is {:?}. \
             Rebase + merge would target the wrong branch. Refusing.",
            worktree_path, worktree_branch, req.branch
        ));
    }

    // reviewer-voice policy voice gate runs AFTER worktree consistency (so we have a
    // valid checkout to diff against `main`) but BEFORE rebase/merge
    // mutate state. Compute carveout eligibility once, hand to
    // [`validate_voices`] for the gate decision.
    let carveout_eligible = branch_carveout_eligible(worktree_path)?;
    validate_voices(req, carveout_eligible)?;

    // Lease-compliance gate (2026-04-29 — closes follow-up-tracker row 6
    // from `dispatch-review-lease-compliance`). For each reviewer voice
    // listed in `req.reviewer_voices` (excluding the `worktree-worker`
    // sentinel), look up the canonical verdict-file path
    // `/tmp/<branch>-<voice>.md`. When present, run
    // `tools/extract-verdict.sh` and parse the JSON. If
    // `lease_compliance == "out-of-scope"`, REJECT the merge regardless
    // of verdict word — SKILL.md `dispatch-review` §"Lease compliance"
    // promises this gate. Parser exit-non-zero → REJECT with
    // `verdict_parse_failed`. Missing verdict file → unchanged behavior
    // (the existing reviewer-trailer hook is the merge-time enforcer
    // for verdict-file presence; this gate only consumes files that
    // exist).
    enforce_lease_compliance(main_repo, req)?;

    // Step 1: snapshot pre-state.
    let pre_main_tip = head_sha(main_repo)?;
    let pre_branch_tip = head_sha(worktree_path)?;

    // Already-merged check. If main already contains branch tip, no
    // work to do.
    if branch_already_merged(main_repo, &pre_branch_tip)? {
        return Ok(json!({
            "status": MergeStatus::AlreadyMerged.as_wire(),
            "pre_state": { "main_tip": pre_main_tip, "branch_tip": pre_branch_tip },
            "post_state": { "main_tip": pre_main_tip },
        }));
    }

    // Dry-run: skip rebase + merge, just compose + write the
    // proposed message.
    if req.dry_run {
        let msg = compose_merge_message(req);
        let proposed = format!("/tmp/merge-msg-{}-DRYRUN.txt", req.branch);
        fs::write(&proposed, &msg)
            .map_err(|e| format!("merge_to_main: write dry-run msg {proposed}: {e}"))?;
        return Ok(json!({
            "status": MergeStatus::DryRun.as_wire(),
            "pre_state": { "main_tip": pre_main_tip, "branch_tip": pre_branch_tip },
            "proposed_message_path": proposed,
            "rebase_conflicts": Value::Array(Vec::new()),
        }));
    }

    // Step 2: rebase branch onto main.
    let rebase_outcome = run_rebase_main(worktree_path, req.auto_resolve_cumulative_md)?;
    if let RebaseOutcome::Conflict { kind, files } = rebase_outcome {
        return Ok(json!({
            "status": MergeStatus::RebaseConflict.as_wire(),
            "rebase_conflicts": files
                .iter()
                .map(|f| json!({
                    "file": f,
                    "conflict_kind": kind.as_wire(),
                }))
                .collect::<Vec<_>>(),
            "cumulative_md_resolved": false,
            "pre_state": { "main_tip": pre_main_tip, "branch_tip": pre_branch_tip },
        }));
    }
    let cumulative_md_resolved = matches!(rebase_outcome, RebaseOutcome::CleanWithAutoResolve);

    // Refresh branch_tip after rebase (commit hashes change).
    let post_rebase_branch_tip = head_sha(worktree_path)?;
    let merge_tip_enforcement = enforce_merge_tip(
        main_repo,
        worktree_path,
        req,
        &pre_branch_tip,
        &post_rebase_branch_tip,
    )?;
    if let TipEnforcement::ContentDrift {
        reviewed_tip,
        post_rebase_tip,
        tree_diff_summary,
    } = &merge_tip_enforcement
    {
        if std::env::var("WTPOOL_ALLOW_TIP_DRIFT").ok().as_deref() != Some("1") {
            let envelope = json!({
                "error": "merge_tip_drift",
                "reviewed_tip": reviewed_tip,
                "post_rebase_tip": post_rebase_tip,
                "tree_diff_summary": tree_diff_summary,
                "details": "rebase produced a content-different tree from the tip the reviewer signed; refusing merge. Set WTPOOL_ALLOW_TIP_DRIFT=1 to override.",
            });
            return Err(serde_json::to_string(&envelope).unwrap_or_default());
        }
    }

    // Step 4 + 5: compose message + run merge.
    let msg = compose_merge_message(req);
    let sha8 = pre_branch_tip.chars().take(8).collect::<String>();
    let msg_path = format!("/tmp/merge-msg-{}-{}.txt", req.branch, sha8);
    fs::write(&msg_path, &msg).map_err(|e| format!("merge_to_main: write {msg_path}: {e}"))?;

    let merge_args = ["merge", "--no-ff", &req.branch, "-F", &msg_path];
    let rendered = render_command(main_repo, &merge_args, true);
    let out = git_with_main_commit_override(main_repo, &merge_args)?;
    if !out.success() {
        // Distinguish hook-blocked from other failure: hook scripts
        // exit non-zero with their identity in stderr. Best-effort
        // — fall through to generic merge failure on no-match.
        let combined = format!("$ {rendered}\n{}{}", out.stdout, out.stderr);
        let status = if combined.contains("ALLOW_MAIN_COMMIT")
            || combined.contains("enforce-main-commit-policy")
            || combined.contains("protect-main-edits")
        {
            MergeStatus::HookBlocked
        } else {
            // Treat as generic merge failure → wrap as RebaseConflict
            // (the only other "blocked but recoverable" status). The
            // caller can rerun once they've resolved.
            MergeStatus::RebaseConflict
        };
        return Ok(json!({
            "status": status.as_wire(),
            "hook_output": combined,
            "pre_state": { "main_tip": pre_main_tip, "branch_tip": pre_branch_tip },
            "cumulative_md_resolved": cumulative_md_resolved,
        }));
    }

    // Step 6: verify.
    let post_main_tip = head_sha(main_repo)?;
    let trailer_present = commit_message_contains_trailer(main_repo, &post_main_tip, req)?;
    let mut payload = json!({
        "status": MergeStatus::Merged.as_wire(),
        "merge_sha": post_main_tip,
        "pre_state": { "main_tip": pre_main_tip, "branch_tip": pre_branch_tip },
        "post_state": { "main_tip": post_main_tip },
        "cumulative_md_resolved": cumulative_md_resolved,
        "merge_message_path": msg_path,
        "rendered_command": rendered,
        "post_rebase_branch_tip": post_rebase_branch_tip,
    });
    if let TipEnforcement::ContentDrift {
        reviewed_tip,
        post_rebase_tip,
        tree_diff_summary,
    } = merge_tip_enforcement
    {
        if let Some(map) = payload.as_object_mut() {
            map.insert(
                "merge_tip_drift_override".into(),
                json!({
                    "reviewed_tip": reviewed_tip,
                    "post_rebase_tip": post_rebase_tip,
                    "tree_diff_summary": tree_diff_summary,
                }),
            );
        }
    }
    if !trailer_present {
        // Idempotency: merge spec + Git Safety Protocol — never undo.
        // Surface the warning, leave the merge in place.
        if let Some(map) = payload.as_object_mut() {
            map.insert(
                "warning".into(),
                json!("Reviewed-by trailer not found in merge commit message; merge was NOT undone (Git Safety Protocol). Inspect commit + amend if necessary."),
            );
        }
    }
    Ok(payload)
}

/// Resolve the absolute path of `tools/extract-verdict.sh` relative to
/// `main_repo`. The MCP server's cwd is not guaranteed to be the repo
/// root, so the lease gate cannot rely on a bare `tools/` prefix —
/// every callsite resolves through `main_repo`. Override with
/// `WTPOOL_EXTRACT_VERDICT_BIN` env var (used by integration tests to
/// point at a deliberately-broken stub for the parse-failure case).
pub(crate) fn extract_verdict_script(main_repo: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("WTPOOL_EXTRACT_VERDICT_BIN") {
        return PathBuf::from(p);
    }
    main_repo.join("tools/extract-verdict.sh")
}

/// Resolve the canonical verdict-file path for a `(branch, voice)`
/// pair: `/tmp/<branch>-<voice>.md`. Override the `/tmp` prefix via
/// `WTPOOL_VERDICT_DIR` env var so integration tests can isolate verdict
/// files in a `TempDir` rather than racing on the global `/tmp`.
pub(crate) fn verdict_file_path(branch: &str, voice: &str) -> PathBuf {
    let dir = std::env::var("WTPOOL_VERDICT_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join(format!("{branch}-{voice}.md"))
}

/// Reviewer voices that should NOT be probed for a verdict file. The
/// `worktree-worker` sentinel exists to satisfy reviewer-voice policy's non-empty
/// trailer requirement when paired with a real reviewer; it never
/// owns a verdict file.
fn voice_skip_for_lease(voice: &str) -> bool {
    let trimmed = voice.trim();
    trimmed.is_empty() || trimmed == "worktree-worker"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TipEnforcement {
    NotApplicable,
    NoRebase,
    ShaShiftIdenticalTree {
        reviewed_tip: String,
        post_rebase_tip: String,
    },
    ContentDrift {
        reviewed_tip: String,
        post_rebase_tip: String,
        tree_diff_summary: String,
    },
}

pub(crate) fn enforce_merge_tip(
    main_repo: &Path,
    worktree_path: &Path,
    req: &MergeRequest,
    pre_rebase_branch_tip: &str,
    post_rebase_branch_tip: &str,
) -> Result<TipEnforcement, String> {
    let script = extract_verdict_script(main_repo);
    let mut saw_verdict = false;
    let mut outcome = TipEnforcement::NotApplicable;
    let post_rebase_tree = rev_parse_tree(worktree_path, post_rebase_branch_tip).map_err(|e| {
        let envelope = json!({
            "error": "post_rebase_tip_unresolvable",
            "post_rebase_tip": post_rebase_branch_tip,
            "details": e,
        });
        serde_json::to_string(&envelope).unwrap_or_default()
    })?;

    for voice in &req.reviewer_voices {
        if voice_skip_for_lease(voice) {
            continue;
        }
        let verdict_path = verdict_file_path(&req.branch, voice.trim());
        if !verdict_path.exists() {
            continue;
        }
        saw_verdict = true;

        let out = match Command::new(&script).arg(&verdict_path).output() {
            Ok(o) => o,
            Err(e) => {
                let envelope = json!({
                    "error": "verdict_parse_failed",
                    "voice": voice.trim(),
                    "verdict_path": verdict_path.display().to_string(),
                    "details": format!("spawn extract-verdict.sh failed: {e}"),
                });
                return Err(serde_json::to_string(&envelope).unwrap_or_default());
            }
        };
        if !out.status.success() {
            let envelope = json!({
                "error": "verdict_parse_failed",
                "voice": voice.trim(),
                "verdict_path": verdict_path.display().to_string(),
                "details": String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
            return Err(serde_json::to_string(&envelope).unwrap_or_default());
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
            let envelope = json!({
                "error": "verdict_parse_failed",
                "voice": voice.trim(),
                "verdict_path": verdict_path.display().to_string(),
                "details": format!("parse JSON from extract-verdict.sh stdout: {e}"),
            });
            serde_json::to_string(&envelope).unwrap_or_default()
        })?;
        let reviewed_tip = parsed
            .get("tip")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let envelope = json!({
                    "error": "verdict_parse_failed",
                    "voice": voice.trim(),
                    "verdict_path": verdict_path.display().to_string(),
                    "details": "extract-verdict.sh JSON missing top-level `tip` string",
                });
                serde_json::to_string(&envelope).unwrap_or_default()
            })?
            .to_string();

        let reviewed_tree = rev_parse_tree(worktree_path, &reviewed_tip).map_err(|e| {
            let envelope = json!({
                "error": "verdict_tip_unresolvable",
                "voice": voice.trim(),
                "verdict_path": verdict_path.display().to_string(),
                "reviewed_tip": reviewed_tip,
                "post_rebase_tip": post_rebase_branch_tip,
                "details": e,
            });
            serde_json::to_string(&envelope).unwrap_or_default()
        })?;

        if reviewed_tree == post_rebase_tree {
            outcome = TipEnforcement::NoRebase;
            continue;
        }
        if reviewed_tip == post_rebase_branch_tip {
            if matches!(
                outcome,
                TipEnforcement::NotApplicable | TipEnforcement::NoRebase
            ) {
                outcome = TipEnforcement::ShaShiftIdenticalTree {
                    reviewed_tip,
                    post_rebase_tip: post_rebase_branch_tip.to_string(),
                };
            }
            continue;
        }

        let tree_diff_summary = diff_stat_summary(main_repo, &reviewed_tip, post_rebase_branch_tip)
            .unwrap_or_else(|_| "tree diff summary unavailable".to_string());
        return Ok(TipEnforcement::ContentDrift {
            reviewed_tip,
            post_rebase_tip: post_rebase_branch_tip.to_string(),
            tree_diff_summary,
        });
    }

    if !saw_verdict {
        return Ok(TipEnforcement::NotApplicable);
    }
    if pre_rebase_branch_tip == post_rebase_branch_tip
        && matches!(outcome, TipEnforcement::NotApplicable)
    {
        return Ok(TipEnforcement::NoRebase);
    }
    Ok(outcome)
}

fn rev_parse_tree(repo: &Path, sha: &str) -> Result<String, String> {
    let treeish = format!("{sha}^{{tree}}");
    let out = git(repo, &["rev-parse", &treeish])?;
    if !out.success() {
        return Err(format!(
            "git rev-parse {treeish} failed in {}: {}",
            repo.display(),
            out.stderr.trim()
        ));
    }
    Ok(out.stdout.trim().to_string())
}

fn diff_stat_summary(repo: &Path, lhs: &str, rhs: &str) -> Result<String, String> {
    let out = git(repo, &["diff", "--stat", lhs, rhs])?;
    if !out.success() {
        return Err(format!(
            "git diff --stat {lhs} {rhs} failed in {}: {}",
            repo.display(),
            out.stderr.trim()
        ));
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("tree contents differ")
        .to_string())
}

/// Pre-rebase lease-compliance gate. Returns `Err(json_string)` with a
/// structured error envelope on rejection so the caller can parse the
/// `error` discriminator. The Err-wrapped JSON shape is the wire
/// contract — tests parse it; do not change shape without bumping the
/// docstring.
///
/// Error kinds:
///
/// - `lease_violation` — at least one reviewer voice's verdict file
///   carried `lease_compliance: out-of-scope`. Merge MUST NOT proceed
///   regardless of verdict word (PROCEED + out-of-scope means the
///   reviewer signed off but flagged the worker overstepped lease;
///   the gate is the policy enforcer).
/// - `verdict_parse_failed` — `extract-verdict.sh` exited non-zero on
///   a verdict file we tried to inspect. Treated as rejection (fail-
///   closed) because we cannot tell whether the file claims
///   out-of-scope.
pub(crate) fn enforce_lease_compliance(main_repo: &Path, req: &MergeRequest) -> Result<(), String> {
    let script = extract_verdict_script(main_repo);
    let mut violations: Vec<(String, PathBuf)> = Vec::new();
    let mut parse_failures: Vec<(String, PathBuf, String)> = Vec::new();

    for voice in &req.reviewer_voices {
        if voice_skip_for_lease(voice) {
            continue;
        }
        let vf = verdict_file_path(&req.branch, voice.trim());
        if !vf.exists() {
            // Missing-verdict-file behavior unchanged: the merge-time
            // reviewer-trailer hook is the canonical presence check.
            continue;
        }
        let out = match Command::new(&script).arg(&vf).output() {
            Ok(o) => o,
            Err(e) => {
                let envelope = json!({
                    "error": "verdict_parse_failed",
                    "voice": voice.trim(),
                    "verdict_path": vf.display().to_string(),
                    "details": format!("spawn extract-verdict.sh failed: {e}"),
                });
                return Err(serde_json::to_string(&envelope).unwrap_or_default());
            }
        };
        if !out.status.success() {
            parse_failures.push((
                voice.trim().to_string(),
                vf.clone(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = match serde_json::from_str(stdout.trim()) {
            Ok(v) => v,
            Err(e) => {
                parse_failures.push((
                    voice.trim().to_string(),
                    vf.clone(),
                    format!("parse JSON from extract-verdict.sh stdout: {e}"),
                ));
                continue;
            }
        };
        let lease = parsed
            .get("lease_compliance")
            .and_then(|v| v.as_str())
            .unwrap_or("not-applicable");
        if lease == "out-of-scope" {
            violations.push((voice.trim().to_string(), vf));
        }
    }

    // Fail-closed precedence: any out-of-scope finding rejects, even if
    // some voices also failed to parse. Surfacing the lease violation
    // is more actionable than the parse failure.
    if let Some((voice, path)) = violations.into_iter().next() {
        let envelope = json!({
            "error": "lease_violation",
            "voice": voice,
            "verdict_path": path.display().to_string(),
            "details": "lease_compliance=out-of-scope",
        });
        return Err(serde_json::to_string(&envelope).unwrap_or_default());
    }
    if let Some((voice, path, details)) = parse_failures.into_iter().next() {
        let envelope = json!({
            "error": "verdict_parse_failed",
            "voice": voice,
            "verdict_path": path.display().to_string(),
            "details": details,
        });
        return Err(serde_json::to_string(&envelope).unwrap_or_default());
    }
    Ok(())
}

/// Encoded outcome of step 2 (`git rebase main`).
enum RebaseOutcome {
    /// Rebase clean, no conflicts.
    Clean,
    /// Rebase had cumulative-doc-only conflict, auto-resolved per §3.5.
    CleanWithAutoResolve,
    /// Rebase had a conflict the tool refuses to resolve. Rebase has
    /// been aborted by the time this is returned.
    Conflict {
        kind: ConflictKind,
        files: Vec<String>,
    },
}

fn run_rebase_main(worktree: &Path, auto_resolve: bool) -> Result<RebaseOutcome, String> {
    let out = git(worktree, &["rebase", "main"])?;
    if out.success() {
        return Ok(RebaseOutcome::Clean);
    }
    // Identify conflict-marker files via `diff --name-only
    // --diff-filter=U`.
    let unmerged = git(worktree, &["diff", "--name-only", "--diff-filter=U"])?;
    let files: Vec<String> = unmerged
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let only_cumulative = files.len() == 1 && files[0] == CUMULATIVE_MD_REL;

    if only_cumulative && auto_resolve {
        // Read the conflicted file, attempt heuristic.
        let p = worktree.join(CUMULATIVE_MD_REL);
        let body = fs::read_to_string(&p)
            .map_err(|e| format!("merge_to_main: read {}: {e}", p.display()))?;
        match resolve_cumulative_md_conflict(&body) {
            Ok(resolved) => {
                fs::write(&p, resolved)
                    .map_err(|e| format!("merge_to_main: write {}: {e}", p.display()))?;
                git(worktree, &["add", CUMULATIVE_MD_REL])?;
                let cont = git(worktree, &["rebase", "--continue"])?;
                if !cont.success() {
                    // Continue still failing → abort + bail.
                    let _ = git(worktree, &["rebase", "--abort"]);
                    return Ok(RebaseOutcome::Conflict {
                        kind: ConflictKind::ContentMixed,
                        files,
                    });
                }
                return Ok(RebaseOutcome::CleanWithAutoResolve);
            }
            Err(kind) => {
                let _ = git(worktree, &["rebase", "--abort"]);
                return Ok(RebaseOutcome::Conflict { kind, files });
            }
        }
    }

    // Anything else: abort.
    let _ = git(worktree, &["rebase", "--abort"]);
    Ok(RebaseOutcome::Conflict {
        kind: ConflictKind::ContentMixed,
        files,
    })
}

fn head_sha(repo: &Path) -> Result<String, String> {
    let out = git(repo, &["rev-parse", "HEAD"])?;
    if !out.success() {
        return Err(format!(
            "git rev-parse HEAD failed in {}: {}",
            repo.display(),
            out.stderr.trim()
        ));
    }
    Ok(out.stdout.trim().to_string())
}

/// Return the symbolic branch name currently checked out in `repo`
/// (e.g. `feature-x`, `main`). Returns `"HEAD"` for a detached HEAD
/// (`git rev-parse --abbrev-ref HEAD` emits literal `HEAD` in that
/// case). Used by the branch ↔ worktree consistency guardrail in
/// [`merge_to_main`].
fn current_branch(repo: &Path) -> Result<String, String> {
    let out = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !out.success() {
        return Err(format!(
            "git rev-parse --abbrev-ref HEAD failed in {}: {}",
            repo.display(),
            out.stderr.trim()
        ));
    }
    Ok(out.stdout.trim().to_string())
}

fn branch_already_merged(main_repo: &Path, branch_tip: &str) -> Result<bool, String> {
    // `git merge-base --is-ancestor <branch_tip> main` exits 0 if
    // ancestor (already-merged), 1 if not.
    let out = git(
        main_repo,
        &["merge-base", "--is-ancestor", branch_tip, "main"],
    )?;
    Ok(out.status == 0)
}

fn commit_message_contains_trailer(
    main_repo: &Path,
    sha: &str,
    req: &MergeRequest,
) -> Result<bool, String> {
    let out = git(main_repo, &["log", "-1", "--format=%B", sha])?;
    if !out.success() {
        return Err(format!("git log on {sha} failed: {}", out.stderr.trim()));
    }
    let trailer = compose_reviewer_trailer(&req.reviewer_voices);
    Ok(out.stdout.contains(&trailer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_default() -> MergeRequest {
        MergeRequest {
            branch: "feature-x".into(),
            reviewer_voices: vec!["torvalds".into(), "lattner".into()],
            merge_message_subject: "feature-x: ship it".into(),
            merge_message_body: "Body.".into(),
            auto_resolve_cumulative_md: true,
            dry_run: false,
        }
    }

    #[test]
    fn validate_request_accepts_empty_voices_defers_to_carveout_gate() {
        // Empty-voice case is conditionally legal (reviewer-voice policy carveout)
        // and must reach validate_voices for the gate decision —
        // validate_request itself is no longer the rejector.
        let mut req = req_default();
        req.reviewer_voices = vec![];
        validate_request(&req).expect("structural validate must accept empty voices");
    }

    #[test]
    fn validate_request_fast_path_rejects_worktree_worker_only() {
        // Defense-in-depth: validate_request rejects
        // worktree-worker-only voices BEFORE the carveout probe spawns
        // a `git diff` subprocess. validate_voices repeats the check
        // afterward. The duplication is intentional — see the comment
        // in validate_request for the rationale (avoid a guaranteed-
        // reject git subprocess; surface the reviewer-voice policy violation even
        // when the worktree probe would itself error out for unrelated
        // reasons). Mutation: drop the fast-path and the
        // mcp::tests::merge_to_main_rejects_self_merge_voices
        // integration test — which uses a non-existent worktree path
        // by default — fails on the missing-worktree git error
        // instead of the reviewer-voice policy self-merge cite.
        let mut req = req_default();
        req.reviewer_voices = vec!["worktree-worker".into()];
        let err = validate_request(&req).unwrap_err();
        assert!(
            err.contains("reviewer-voice policy") && err.contains("self-merge prohibited"),
            "expected reviewer-voice policy self-merge cite, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_torvalds_alone() {
        let mut req = req_default();
        req.reviewer_voices = vec!["torvalds".into()];
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn validate_rejects_subject_over_72_chars() {
        let mut req = req_default();
        req.merge_message_subject = "x".repeat(73);
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validate_rejects_blank_subject() {
        let mut req = req_default();
        req.merge_message_subject = "   ".into();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validate_rejects_blank_branch() {
        let mut req = req_default();
        req.branch = "".into();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn compose_trailer_two_voices_one_line_each() {
        let t = compose_reviewer_trailer(&["torvalds".into(), "lattner".into()]);
        assert_eq!(t, "Reviewed-by: torvalds\nReviewed-by: lattner");
    }

    #[test]
    fn compose_trailer_preserves_explicit_human_label() {
        let t = compose_reviewer_trailer(&["human".into(), "torvalds".into()]);
        assert_eq!(t, "Reviewed-by: human\nReviewed-by: torvalds");
    }

    #[test]
    fn compose_trailer_preserves_three_panel() {
        let t = compose_reviewer_trailer(&["three-panel".into()]);
        assert_eq!(t, "Reviewed-by: three-panel");
    }

    #[test]
    fn compose_trailer_passes_voices_through_verbatim() {
        // No -agent suffix appended; allowlist enforcement is the
        // caller's / hook's job, not the composer's.
        let t = compose_reviewer_trailer(&["mechanical".into()]);
        assert_eq!(t, "Reviewed-by: mechanical");
    }

    #[test]
    fn compose_message_includes_subject_body_trailer() {
        let req = req_default();
        let m = compose_merge_message(&req);
        assert!(m.starts_with("feature-x: ship it\n\n"));
        assert!(m.contains("Body."));
        assert!(m
            .trim_end()
            .ends_with("Reviewed-by: torvalds\nReviewed-by: lattner"));
    }

    #[test]
    fn compose_message_omits_blank_body() {
        let mut req = req_default();
        req.merge_message_body = "".into();
        let m = compose_merge_message(&req);
        // No double-blank between subject and trailer when body absent.
        assert!(m.contains("feature-x: ship it\n\nReviewed-by:"));
    }

    #[test]
    fn validate_rejects_subject_without_branch_name() {
        // Mutation probe: comment out the subject↔branch check in
        // `validate_request` and this assertion breaks (subject is
        // otherwise legal: 8 chars, non-blank, voices include
        // torvalds).
        let mut req = req_default();
        req.branch = "feature-x".into();
        req.merge_message_subject = "ship it".into(); // no "feature-x"
        let err = validate_request(&req).unwrap_err();
        assert!(
            err.contains("must contain `branch`") && err.contains("Rule 10"),
            "expected Rule 10 subject/branch error, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_subject_containing_branch_name() {
        let mut req = req_default();
        req.branch = "feature-x".into();
        req.merge_message_subject = "feature-x: ship it".into();
        validate_request(&req).expect("subject contains branch substring");
    }

    #[test]
    fn validate_accepts_when_bypass_env_set() {
        // Serialise via a process-wide guard: env mutations from
        // concurrent tests can race (cargo test runs threads in the
        // same process). We use a Mutex to make this test's window
        // deterministic; the bypass codepath itself reads via
        // `std::env::var` per call.
        use std::sync::{Mutex, OnceLock};
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().unwrap();

        let mut req = req_default();
        req.branch = "feature-x".into();
        req.merge_message_subject = "alt-name: deliberate rename".into();
        // Without bypass: rejected.
        std::env::remove_var("BYPASS_SUBJECT_BRANCH_CHECK");
        assert!(
            validate_request(&req).is_err(),
            "no-bypass control: must reject"
        );
        // With bypass=1: accepted.
        std::env::set_var("BYPASS_SUBJECT_BRANCH_CHECK", "1");
        let r = validate_request(&req);
        std::env::remove_var("BYPASS_SUBJECT_BRANCH_CHECK");
        r.expect("bypass=1 must allow non-substring subject");
    }

    #[test]
    fn validate_bypass_only_accepts_literal_one() {
        // Mutation probe: change `Some("1")` to `Some(_)` and this
        // test fails — `BYPASS=true`, `BYPASS=yes` must NOT bypass.
        use std::sync::{Mutex, OnceLock};
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = GUARD.get_or_init(|| Mutex::new(())).lock().unwrap();

        let mut req = req_default();
        req.branch = "feature-x".into();
        req.merge_message_subject = "alt-name: rename".into();
        for bogus in ["true", "yes", "0", ""] {
            std::env::set_var("BYPASS_SUBJECT_BRANCH_CHECK", bogus);
            let r = validate_request(&req);
            std::env::remove_var("BYPASS_SUBJECT_BRANCH_CHECK");
            assert!(
                r.is_err(),
                "BYPASS={bogus:?} must NOT bypass (only literal '1' bypasses)"
            );
        }
    }

    // ---- reviewer-voice policy carveout (low-blast-radius path allowlist) ---------

    #[test]
    fn carveout_eligible_when_all_paths_allowlisted() {
        // (a) Mixed allowlisted entries: benchmark + ledger +
        // followup-tracker glob match + STARTUP.md.
        let paths = vec![
            "benchmark/foo.py".into(),
            "project/shared/agent-ledger.md".into(),
            "project/shared/followup-tracker-2026-05-04.md".into(),
            "STARTUP.md".into(),
            "tools/routing-policy.md".into(),
        ];
        assert!(
            is_carveout_eligible(&paths),
            "all-allowlist should be carveout-eligible"
        );
    }

    #[test]
    fn carveout_ineligible_for_docs_plans_and_research() {
        // CLAUDE.md Rule 13: docs/plans/** and docs/research/** are
        // EXPLICITLY NOT in the carveout — both require single-voice
        // review (lattner / torvalds respectively). This test pins
        // that exclusion: a regression that adds either path to
        // RULE13_CARVEOUT_ALLOWLIST will flip these asserts and fail
        // the suite. Negative cases on `core/src/lib.rs` cover the
        // baseline non-doc shape.
        for p in [
            "docs/plans/2026-04-25-cropout-phase2.md",
            "docs/plans/foo.md",
            "docs/research/2026-05-04-libgit2-investigation.md",
            "docs/research/y.md",
            "project/shared/some-other-shared-doc.md",
            "core/src/lib.rs",
        ] {
            let paths = vec![p.into()];
            assert!(
                !is_carveout_eligible(&paths),
                "{p} must NOT be carveout-eligible per CLAUDE.md Rule 13"
            );
        }
    }

    #[test]
    fn carveout_ineligible_when_one_path_outside_allowlist() {
        // (b) Single non-allowlisted entry mixed with legal ones.
        // Mutation probe: change `.all` to `.any` in is_carveout_eligible
        // and this test fails.
        let paths = vec![
            "benchmark/foo.py".into(),
            "core/src/foo.rs".into(), // not allowlisted
        ];
        assert!(
            !is_carveout_eligible(&paths),
            "any non-allowlisted path makes branch carveout-INELIGIBLE"
        );
    }

    #[test]
    fn carveout_ineligible_on_empty_input() {
        // (d) precondition: empty diff → not eligible (caller short-
        // circuits as AlreadyMerged anyway, but explicit is safer).
        assert!(!is_carveout_eligible(&[]));
    }

    #[test]
    fn carveout_rejects_paths_outside_allowlist() {
        // Random non-allowlisted file shape — code, configs, etc. —
        // must NOT be carveout-eligible. Pin a few representative
        // shapes so a future allowlist expansion has to confront
        // the rationale.
        for p in [
            "src/foo.rs",
            "Cargo.toml",
            ".github/workflows/ci.yml",
            "tools/loader.py",
        ] {
            let paths = vec![p.into()];
            assert!(
                !is_carveout_eligible(&paths),
                "{p} must NOT be carveout-eligible"
            );
        }
    }

    #[test]
    fn carveout_rejects_blank_path_entries() {
        // Defensive: a blank entry is not "trivially allowlisted".
        let paths = vec!["benchmark/foo.py".into(), "".into()];
        assert!(!is_carveout_eligible(&paths));
    }

    #[test]
    fn validate_voices_accepts_named_voice_regardless_of_carveout() {
        // Explicit reviewer named → carveout state irrelevant.
        let mut req = req_default();
        req.reviewer_voices = vec!["torvalds".into()];
        validate_voices(&req, false).expect("named voice must always be legal");
        validate_voices(&req, true).expect("named voice must always be legal");
    }

    #[test]
    fn validate_voices_rejects_empty_when_not_carveout() {
        let mut req = req_default();
        req.reviewer_voices = vec![];
        let err = validate_voices(&req, false).unwrap_err();
        assert!(
            err.contains("reviewer-voice policy") && err.contains("carveout"),
            "expected reviewer-voice policy + carveout cite, got: {err}"
        );
    }

    #[test]
    fn validate_voices_accepts_empty_when_carveout_eligible() {
        let mut req = req_default();
        req.reviewer_voices = vec![];
        validate_voices(&req, true).expect("empty voices on carveout branch must be legal");
    }

    #[test]
    fn validate_voices_rejects_only_worktree_worker_even_on_carveout() {
        // Self-merge prohibition (reviewer-voice policy spirit) trumps carveout
        // optimisation — worktree-worker self-attestation is never a
        // legal substitute, even on an allowlisted branch.
        let mut req = req_default();
        req.reviewer_voices = vec!["worktree-worker".into()];
        let err = validate_voices(&req, true).unwrap_err();
        assert!(
            err.contains("self-merge prohibited"),
            "expected self-merge cite, got: {err}"
        );
    }

    #[test]
    fn carveout_trailer_emitted_when_voices_empty() {
        let trailer = compose_reviewer_trailer(&[]);
        assert_eq!(trailer, RULE13_CARVEOUT_TRAILER);
        assert_eq!(trailer, "Reviewed-by: rule-13-carveout");
    }

    #[test]
    fn carveout_trailer_appears_in_merge_message_when_voices_empty() {
        let mut req = req_default();
        req.reviewer_voices = vec![];
        let m = compose_merge_message(&req);
        assert!(
            m.trim_end().ends_with("Reviewed-by: rule-13-carveout"),
            "merge message must end with carveout trailer:\n{m}"
        );
    }

    #[test]
    fn merge_status_wire_strings_stable() {
        assert_eq!(MergeStatus::Merged.as_wire(), "merged");
        assert_eq!(MergeStatus::RebaseConflict.as_wire(), "rebase_conflict");
        assert_eq!(MergeStatus::TestGateFailed.as_wire(), "test_gate_failed");
        assert_eq!(MergeStatus::HookBlocked.as_wire(), "hook_blocked");
        assert_eq!(MergeStatus::AlreadyMerged.as_wire(), "already_merged");
        assert_eq!(MergeStatus::DryRun.as_wire(), "dry_run");
    }
}
