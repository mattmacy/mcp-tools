//! `pending_review` backend.
//!
//! Stats the canonical reviewer-verdict-file paths
//! `/tmp/<branch>-torvalds.md` and `/tmp/<branch>-lattner.md`. These
//! are the conventional drop-points where reviewer agents leave their
//! verdict; the merge gate (CLAUDE.md Rule 13) requires both to exist
//! before main can accept the branch.
//!
//! Output per spec §1.2:
//! ```text
//! { torvalds: { exists, mtime, verdict_word? } | null,
//!   lattner:  { exists, mtime, verdict_word? } | null }
//! ```
//!
//! `verdict_word` extraction: per spec, "first whitespace token of
//! file's first line, lowercased". The canonical reviewer voices
//! today open with `VERDICT: PROCEED` (or `…BOUNCE_BACK`,
//! `…REVERT`, etc.) — strict spec interpretation gives `verdict:` for
//! every file, which is useless. We normalise: if the literal first
//! token (lowercased) is `verdict:` (i.e., the canonical header), we
//! return the SECOND whitespace token instead. Otherwise we return
//! the first token verbatim. Both branches lowercase + strip a
//! trailing `:` so callers see uniform values.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Reviewer voices the `pending_review` tool stats. Centralised here
/// so future voices (`three-panel`, `human`, …) land in one place.
/// `carmack` is now common across phase-2 perf reviews and is included
/// alongside the original torvalds + lattner pair; consumers that only
/// inspected the previous two voices keep working (additive output
/// keys) and gain the carmack signal as a bonus.
pub(crate) const REVIEWER_VOICES: &[&str] = &["torvalds", "lattner", "carmack"];

/// `pending_review` body for one branch.
pub fn pending_review(branch: &str) -> Result<Value, String> {
    if branch.is_empty() {
        return Err("pending_review: empty branch name".into());
    }
    if !branch.chars().all(is_acceptable_branch_char) {
        return Err(format!(
            "pending_review: branch name {branch:?} contains rejected characters; \
             expected ASCII alnum / dash / underscore / slash / dot"
        ));
    }
    // Path-traversal guard: `..` segment in a branch name would let a
    // caller smuggle `/tmp/<branch>-torvalds.md` into ascending the
    // tree. Branches with literal `..` are not legal in git anyway.
    if branch.contains("..") || branch.starts_with('-') {
        return Err(format!(
            "pending_review: branch name {branch:?} rejected; \
             contains `..` or leading `-`"
        ));
    }
    let mut out = serde_json::Map::new();
    for voice in REVIEWER_VOICES {
        let path = verdict_path(branch, voice);
        out.insert(voice.to_string(), describe_verdict_file(&path));
    }
    Ok(Value::Object(out))
}

/// Construct `/tmp/<branch>-<voice>.md`. Public so the CLI can echo it.
pub fn verdict_path(branch: &str, voice: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/{branch}-{voice}.md"))
}

fn describe_verdict_file(path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let verdict_word = first_line(path)
                .as_deref()
                .map(extract_verdict_word)
                .unwrap_or_default();
            json!({
                "exists": true,
                "path": path,
                "mtime_unix": mtime,
                "mtime_iso": iso8601(meta.modified().unwrap_or(UNIX_EPOCH)),
                "verdict_word": verdict_word,
            })
        }
        Err(_) => json!({
            "exists": false,
            "path": path,
            "mtime_unix": null,
            "mtime_iso": null,
            "verdict_word": null,
        }),
    }
}

fn first_line(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    Some(s.lines().next().unwrap_or("").to_string())
}

/// Extract the verdict word per the "first whitespace token, lowercased"
/// rule, with the `VERDICT: <X>` header normalisation described in the
/// module-level comment.
pub(crate) fn extract_verdict_word(line: &str) -> String {
    let mut tokens = line.split_whitespace();
    let first = tokens.next().unwrap_or("").to_lowercase();
    let first_no_colon = first.trim_end_matches(':');
    if first_no_colon == "verdict" {
        // Canonical form: `VERDICT: PROCEED`.
        let second = tokens.next().unwrap_or("");
        return second.trim_end_matches(':').to_lowercase();
    }
    first_no_colon.to_string()
}

fn is_acceptable_branch_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.')
}

fn iso8601(t: SystemTime) -> String {
    crate::agents::iso8601_for_test(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_verdict_word_canonical_proceed() {
        assert_eq!(extract_verdict_word("VERDICT: PROCEED"), "proceed");
        assert_eq!(
            extract_verdict_word("VERDICT: PROCEED_WITH_FOLLOWUP"),
            "proceed_with_followup"
        );
    }

    #[test]
    fn extract_verdict_word_first_token_when_no_header() {
        assert_eq!(
            extract_verdict_word("BOUNCE_BACK because reasons"),
            "bounce_back"
        );
        assert_eq!(extract_verdict_word("ship it"), "ship");
    }

    #[test]
    fn extract_verdict_word_handles_markdown_heading() {
        // Some reviewers open with `# lattner verdict ...` — first
        // token is `#`, returned verbatim (lowercase). Caller can
        // recognise this as "no header detected".
        assert_eq!(extract_verdict_word("# lattner verdict — branch"), "#");
    }

    #[test]
    fn extract_verdict_word_handles_empty_line() {
        assert_eq!(extract_verdict_word(""), "");
    }

    #[test]
    fn pending_review_rejects_empty_branch() {
        assert!(pending_review("").is_err());
    }

    #[test]
    fn pending_review_rejects_branch_with_shell_metachars() {
        assert!(pending_review("foo;rm -rf /").is_err());
        assert!(pending_review("../escape").is_err());
        assert!(pending_review("$(cmd)").is_err());
    }

    #[test]
    fn pending_review_missing_files_returns_exists_false() {
        let v = pending_review("definitely-not-a-real-branch-pq8w7r6").unwrap();
        assert_eq!(v["torvalds"]["exists"], false);
        assert_eq!(v["lattner"]["exists"], false);
        assert_eq!(v["carmack"]["exists"], false);
        assert_eq!(v["torvalds"]["verdict_word"], serde_json::Value::Null);
    }

    #[test]
    fn pending_review_keys_include_carmack() {
        // Regression pin for REVIEWER_VOICES extension: the third
        // voice MUST appear in the output, otherwise the wire schema
        // silently regressed to torvalds+lattner only.
        let v = pending_review("any-branch-name-here-mp9w2x").unwrap();
        let map = v.as_object().expect("object");
        assert!(
            map.contains_key("carmack"),
            "missing carmack key in {map:?}"
        );
    }

    #[test]
    fn pending_review_finds_present_verdict_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        // We can't reroute /tmp inside the prod helper, but we can
        // exercise `describe_verdict_file` directly.
        let path = tmp.path().join("foo-torvalds.md");
        fs::write(&path, "VERDICT: BOUNCE_BACK\n\nrationale\n").unwrap();
        let v = describe_verdict_file(&path);
        assert_eq!(v["exists"], true);
        assert_eq!(v["verdict_word"], "bounce_back");
    }
}
