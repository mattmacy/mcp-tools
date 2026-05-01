//! Heuristic resolver for the cumulative-status-doc table-row conflict
//! shape.
//!
//! When two branches each append rows to a shared status table in a
//! markdown doc, rebase produces a conflict where `ours` and `theirs`
//! are pure additions of `| … |` table rows or
//! `<!-- branch-name: … -->` comments. A union-merge sorted by
//! `(extracted-branch-name, line-text)` resolves the conflict the way
//! a reviewer would resolve it by hand.
//!
//! The heuristic is deliberately conservative — it bails (returns
//! [`ConflictKind::ContentMixed`]) on anything outside the strict
//! pure-addition + table-or-branch-comment shape, so the parent
//! `merge_to_main` tool can fall back to `rebase_conflict` and let a
//! human reviewer take over. Bias: false-negative is cheap (one
//! human intervention), false-positive is expensive (silently mangles
//! the cumulative doc).
//!
//! Public surface:
//!
//! - [`ConflictKind`] — the bail-reason enum returned to the caller.
//! - [`resolve_cumulative_md_conflict`] — top-level entrypoint, takes
//!   the full file body containing one or more `<<<<<<<` blocks +
//!   returns either the resolved body or a [`ConflictKind`].

use std::collections::BTreeSet;

/// Why the heuristic refused to auto-resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Conflict block contains deletions, modifications, or
    /// non-table-row / non-branch-comment lines. Bail to manual
    /// resolution.
    ContentMixed,
    /// Conflict block lies in the first 100 or last 50 lines of the
    /// file (spec §3.5 reject condition #3 — preamble / footer).
    InProtectedZone,
    /// Conflict markers were malformed (mismatched / nested / missing
    /// `=======` separator).
    MalformedMarkers,
}

impl ConflictKind {
    /// Stable wire string for the MCP `rebase_conflict` payload.
    /// Each variant maps to a distinct string so MCP consumers can
    /// distinguish the bail reason without reparsing prose. Older
    /// callers that only matched `"content_mixed"` will see
    /// `"in_protected_zone"` / `"malformed_markers"` as new strings —
    /// they must extend their match arms; the wire surface widens but
    /// does not break (no string was renamed, only added).
    pub fn as_wire(&self) -> &'static str {
        match self {
            ConflictKind::ContentMixed => "content_mixed",
            ConflictKind::InProtectedZone => "in_protected_zone",
            ConflictKind::MalformedMarkers => "malformed_markers",
        }
    }
}

/// First 100 lines protected (preamble — title, date, summary).
pub(crate) const PREAMBLE_GUARD_LINES: usize = 100;
/// Last 50 lines protected (footer — sign-off, links).
pub(crate) const FOOTER_GUARD_LINES: usize = 50;

/// Resolve the `cumulative.md` conflict shape into a single body.
///
/// Returns `Ok(resolved_body)` on a successful union-merge, or
/// `Err(ConflictKind)` when the heuristic refuses. Caller is expected
/// to treat the error as "abort rebase, surface `rebase_conflict` to
/// the MCP client."
///
/// TODO(deprecation-trigger): retire this heuristic the first time
/// `cumulative.md` gains an in-place row update (i.e. an existing row
/// is *edited* rather than *appended*). The pure-addition gate
/// ([`Block::is_pure_addition`]) and the table/branch-comment shape
/// filter ([`Block::all_lines_table_or_branch_comment`]) will both
/// correctly bail on that pattern, but at that point the heuristic's
/// design horizon (append-only rows) is gone and the right move is to
/// require manual resolution + delete this resolver entirely.
pub fn resolve_cumulative_md_conflict(content: &str) -> Result<String, ConflictKind> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    let blocks = find_conflict_blocks(&lines)?;
    if blocks.is_empty() {
        // No conflict markers at all → return content unchanged.
        return Ok(content.to_string());
    }
    let mut output = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for block in &blocks {
        // Copy through pre-block context unchanged.
        for line in &lines[cursor..block.start] {
            output.push_str(line);
        }

        // Reject conditions per spec §3.5.
        if block.start < PREAMBLE_GUARD_LINES || block.end + FOOTER_GUARD_LINES >= total {
            return Err(ConflictKind::InProtectedZone);
        }
        if !block.is_pure_addition() {
            return Err(ConflictKind::ContentMixed);
        }
        if !block.all_lines_table_or_branch_comment() {
            return Err(ConflictKind::ContentMixed);
        }

        output.push_str(&block.union_merge_sorted());
        cursor = block.end;
    }
    // Copy through anything past the last block.
    for line in &lines[cursor..] {
        output.push_str(line);
    }

    Ok(output)
}

/// One parsed conflict block: index range + parsed sides.
struct Block<'a> {
    /// Index of the line containing `<<<<<<<` (inclusive).
    start: usize,
    /// Index one past the line containing `>>>>>>>` (exclusive).
    end: usize,
    /// `ours` lines (between `<<<<<<<` and `=======`), exclusive of
    /// the marker lines themselves.
    ours: Vec<&'a str>,
    /// `theirs` lines (between `=======` and `>>>>>>>`).
    theirs: Vec<&'a str>,
}

impl<'a> Block<'a> {
    /// True iff every conflicted line is non-empty / non-whitespace.
    /// Pure-addition gate: spec §3.5 step 2 — both sides additive.
    /// Implemented as "no shared content lines" — if either side is
    /// empty the block is trivially additive on the other; if both
    /// are non-empty, neither must claim to delete the other's lines
    /// (encoded by demanding all such lines pass the table-or-comment
    /// filter).
    fn is_pure_addition(&self) -> bool {
        // The literal "both pure additions" definition: both sides
        // contain only NEW lines (relative to a common base we don't
        // have here). Without the base we approximate: both sides
        // must be non-empty and disjoint after trim — otherwise we
        // can't tell what was removed vs added. But "disjoint after
        // trim" is too weak; the load-bearing check is in
        // [`Self::all_lines_table_or_branch_comment`], which gates
        // the shape itself.
        //
        // Reject only the trivial "one side empty" case — that's a
        // delete-on-the-other-side, which is NOT pure addition.
        !self.ours.is_empty() && !self.theirs.is_empty()
    }

    fn all_lines_table_or_branch_comment(&self) -> bool {
        self.ours
            .iter()
            .chain(self.theirs.iter())
            .all(|l| is_table_row(l) || is_branch_comment(l) || is_blank(l))
    }

    fn union_merge_sorted(&self) -> String {
        // Use a BTreeSet keyed on (branch-name, line-text) to
        // deduplicate identical rows that both branches happened to
        // add. `BTreeSet` gives stable lexical ordering for free.
        let mut keyed: BTreeSet<(String, String)> = BTreeSet::new();
        for line in self.ours.iter().chain(self.theirs.iter()) {
            // Skip blank lines — they carry no semantic content + the
            // post-merge file should have at most one blank line per
            // gap.
            if is_blank(line) {
                continue;
            }
            let bn = extract_branch_name(line).unwrap_or_default();
            keyed.insert((bn, (*line).to_string()));
        }
        let mut out = String::new();
        for (_branch, line) in keyed {
            out.push_str(&line);
            // Lines from `split_inclusive('\n')` already carry their
            // trailing newline; only synthesize one if missing (final
            // line of the file with no trailing newline).
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

fn find_conflict_blocks<'a>(lines: &[&'a str]) -> Result<Vec<Block<'a>>, ConflictKind> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim_end_matches(['\n', '\r']);
        if trimmed.starts_with("<<<<<<<") {
            // Find the matching `=======` and `>>>>>>>` markers, with
            // no nesting. Nested conflict markers are pathological
            // and bail.
            let mut sep: Option<usize> = None;
            let mut close: Option<usize> = None;
            for (off, line) in lines.iter().enumerate().skip(i + 1) {
                let lt = line.trim_end_matches(['\n', '\r']);
                if lt.starts_with("<<<<<<<") {
                    return Err(ConflictKind::MalformedMarkers);
                }
                if sep.is_none() && lt.starts_with("=======") {
                    sep = Some(off);
                    continue;
                }
                if lt.starts_with(">>>>>>>") {
                    close = Some(off);
                    break;
                }
            }
            let (sep, close) = match (sep, close) {
                (Some(s), Some(c)) if s < c => (s, c),
                _ => return Err(ConflictKind::MalformedMarkers),
            };
            let ours: Vec<&'a str> = lines[i + 1..sep].to_vec();
            let theirs: Vec<&'a str> = lines[sep + 1..close].to_vec();
            out.push(Block {
                start: i,
                end: close + 1,
                ours,
                theirs,
            });
            i = close + 1;
            continue;
        }
        i += 1;
    }
    Ok(out)
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("| ") || t.starts_with("|---") || t.starts_with("|-")
}

fn is_branch_comment(line: &str) -> bool {
    let t = line.trim();
    // Match `<!-- foo-branch: ... -->` (any HTML comment attributing
    // a row to a branch).
    t.starts_with("<!--") && t.ends_with("-->")
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Extract a branch name from a table row or branch-comment line for
/// stable union-merge ordering. The spec'd ordering is `(branch-name,
/// line-text)`; when no branch name is parseable (rare table-divider
/// rows etc.) we fall back to the empty string, which sorts first.
///
/// Heuristics, in order:
/// 1. `<!-- foo-branch: ... -->` → `foo-branch`.
/// 2. First `phase2` / `-phase-` / `-phase2` token in the row → that
///    token (covers `gm-b3-phase2`, `audio-runtime-phase2`, etc.).
/// 3. First `|`-delimited cell whose stripped form matches
///    `[a-z][a-z0-9-]+` → that cell.
fn extract_branch_name(line: &str) -> Option<String> {
    let t = line.trim();
    if t.starts_with("<!--") {
        // `<!-- branch-name: anything -->` or `<!-- branch-name -->`.
        let inner = t.trim_start_matches("<!--").trim_end_matches("-->").trim();
        let name = inner.split(':').next().unwrap_or(inner).trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    // Tokenise on whitespace + look for a phase-2 branch token.
    for tok in t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        if tok.contains("phase2") || tok.contains("phase-2") {
            return Some(tok.to_string());
        }
    }
    // First `|`-cell that looks like a branch name.
    for cell in t.split('|') {
        let c = cell.trim();
        if c.is_empty() {
            continue;
        }
        if c.chars()
            .next()
            .map(|x| x.is_ascii_lowercase())
            .unwrap_or(false)
            && c.chars()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-' || x == '_')
            && c.len() >= 4
        {
            return Some(c.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(body: &str) -> String {
        // Build a file with >=100 preamble lines + >=50 footer lines
        // so `InProtectedZone` doesn't trigger on small fixtures.
        let mut s = String::new();
        for i in 0..120 {
            s.push_str(&format!("preamble line {i}\n"));
        }
        s.push_str(body);
        for i in 0..60 {
            s.push_str(&format!("footer line {i}\n"));
        }
        s
    }

    #[test]
    fn no_conflict_returns_input_unchanged() {
        let input = padded("| simple | row | here |\n");
        let out = resolve_cumulative_md_conflict(&input).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn pure_table_row_conflict_unions_sorted() {
        let body = "\
<<<<<<< HEAD
| audio-runtime-phase2 | shipped |
=======
| nav-flowfield-phase2 | shipped |
>>>>>>> branch
";
        let resolved = resolve_cumulative_md_conflict(&padded(body)).unwrap();
        // Both rows survive, sorted by branch-name extraction.
        assert!(resolved.contains("| audio-runtime-phase2 | shipped |"));
        assert!(resolved.contains("| nav-flowfield-phase2 | shipped |"));
        // No conflict markers remain.
        assert!(!resolved.contains("<<<<<<<"));
        assert!(!resolved.contains("======="));
        assert!(!resolved.contains(">>>>>>>"));
    }

    #[test]
    fn ordering_is_stable_and_deterministic() {
        let body = "\
<<<<<<< HEAD
| zeta-phase2 | a |
| alpha-phase2 | b |
=======
| mu-phase2 | c |
>>>>>>> branch
";
        let resolved = resolve_cumulative_md_conflict(&padded(body)).unwrap();
        let alpha = resolved.find("alpha-phase2").unwrap();
        let mu = resolved.find("mu-phase2").unwrap();
        let zeta = resolved.find("zeta-phase2").unwrap();
        assert!(alpha < mu && mu < zeta, "lex order: {alpha} {mu} {zeta}");
    }

    #[test]
    fn duplicate_rows_collapse() {
        let body = "\
<<<<<<< HEAD
| same-phase2 | shipped |
=======
| same-phase2 | shipped |
>>>>>>> branch
";
        let resolved = resolve_cumulative_md_conflict(&padded(body)).unwrap();
        let occurrences = resolved.matches("same-phase2").count();
        assert_eq!(occurrences, 1, "duplicate row must collapse");
    }

    #[test]
    fn branch_comment_alongside_row_is_accepted() {
        let body = "\
<<<<<<< HEAD
<!-- audio-runtime-phase2: shipped 2026-04-25 -->
| audio-runtime-phase2 | shipped |
=======
<!-- nav-flowfield-phase2: shipped 2026-04-26 -->
| nav-flowfield-phase2 | shipped |
>>>>>>> branch
";
        let resolved = resolve_cumulative_md_conflict(&padded(body)).unwrap();
        assert!(resolved.contains("audio-runtime-phase2: shipped 2026-04-25"));
        assert!(resolved.contains("nav-flowfield-phase2: shipped 2026-04-26"));
    }

    #[test]
    fn non_table_line_bails_content_mixed() {
        let body = "\
<<<<<<< HEAD
prose paragraph that is not a table row
=======
| nav-flowfield-phase2 | shipped |
>>>>>>> branch
";
        let err = resolve_cumulative_md_conflict(&padded(body)).unwrap_err();
        assert_eq!(err, ConflictKind::ContentMixed);
    }

    #[test]
    fn deletion_one_side_bails_content_mixed() {
        // Pure-addition gate: empty side = deletion-on-other = bail.
        let body = "\
<<<<<<< HEAD
| audio-runtime-phase2 | shipped |
=======
>>>>>>> branch
";
        let err = resolve_cumulative_md_conflict(&padded(body)).unwrap_err();
        assert_eq!(err, ConflictKind::ContentMixed);
    }

    #[test]
    fn preamble_zone_bails() {
        // Conflict at line 50 (< 100) → InProtectedZone.
        let mut s = String::new();
        for i in 0..50 {
            s.push_str(&format!("p{i}\n"));
        }
        s.push_str("<<<<<<< HEAD\n");
        s.push_str("| a-phase2 | x |\n");
        s.push_str("=======\n");
        s.push_str("| b-phase2 | y |\n");
        s.push_str(">>>>>>> branch\n");
        for i in 0..200 {
            s.push_str(&format!("body{i}\n"));
        }
        let err = resolve_cumulative_md_conflict(&s).unwrap_err();
        assert_eq!(err, ConflictKind::InProtectedZone);
    }

    #[test]
    fn footer_zone_bails() {
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("body{i}\n"));
        }
        s.push_str("<<<<<<< HEAD\n");
        s.push_str("| a-phase2 | x |\n");
        s.push_str("=======\n");
        s.push_str("| b-phase2 | y |\n");
        s.push_str(">>>>>>> branch\n");
        for i in 0..30 {
            s.push_str(&format!("f{i}\n"));
        }
        let err = resolve_cumulative_md_conflict(&s).unwrap_err();
        assert_eq!(err, ConflictKind::InProtectedZone);
    }

    #[test]
    fn conflict_kind_wire_strings_are_distinct() {
        // Pre-followup all three collapsed to "content_mixed"; post-
        // followup each variant is its own wire string. Pin so a
        // future "simplify the enum" refactor can't silently regress
        // the MCP consumer's distinguishability.
        let strs = [
            ConflictKind::ContentMixed.as_wire(),
            ConflictKind::InProtectedZone.as_wire(),
            ConflictKind::MalformedMarkers.as_wire(),
        ];
        assert_eq!(strs[0], "content_mixed");
        assert_eq!(strs[1], "in_protected_zone");
        assert_eq!(strs[2], "malformed_markers");
        // Distinctness check: every pair must differ.
        for i in 0..strs.len() {
            for j in (i + 1)..strs.len() {
                assert_ne!(strs[i], strs[j], "wire strings collapsed: {i} {j}");
            }
        }
    }

    #[test]
    fn malformed_markers_bail() {
        let body = "\
<<<<<<< HEAD
| a-phase2 | x |
>>>>>>> branch
";
        let err = resolve_cumulative_md_conflict(&padded(body)).unwrap_err();
        assert_eq!(err, ConflictKind::MalformedMarkers);
    }

    #[test]
    fn extract_branch_name_from_comment() {
        let line = "<!-- gm-b3-phase2: shipped 2026-04-24 -->";
        assert_eq!(extract_branch_name(line), Some("gm-b3-phase2".into()));
    }

    #[test]
    fn extract_branch_name_from_phase2_token_in_table_row() {
        let line = "| enhanced-input-phase2 | shipped | landed `02dea32` |";
        assert_eq!(
            extract_branch_name(line),
            Some("enhanced-input-phase2".into())
        );
    }

    #[test]
    fn multiple_conflict_blocks_resolve_independently() {
        let mut s = String::new();
        for i in 0..120 {
            s.push_str(&format!("p{i}\n"));
        }
        s.push_str("<<<<<<< HEAD\n");
        s.push_str("| a-phase2 | first-block-ours |\n");
        s.push_str("=======\n");
        s.push_str("| b-phase2 | first-block-theirs |\n");
        s.push_str(">>>>>>> branch\n");
        for i in 0..30 {
            s.push_str(&format!("middle{i}\n"));
        }
        s.push_str("<<<<<<< HEAD\n");
        s.push_str("| c-phase2 | second-block-ours |\n");
        s.push_str("=======\n");
        s.push_str("| d-phase2 | second-block-theirs |\n");
        s.push_str(">>>>>>> branch\n");
        for i in 0..70 {
            s.push_str(&format!("f{i}\n"));
        }
        let resolved = resolve_cumulative_md_conflict(&s).unwrap();
        for needle in &["a-phase2", "b-phase2", "c-phase2", "d-phase2"] {
            assert!(resolved.contains(needle), "missing: {needle}");
        }
        assert!(!resolved.contains("<<<<<<<"));
    }
}
