//! Worktree lease — JSON contract that constrains a Codex or
//! cheap-Claude worker to a scoped set of paths and test commands.
//!
//! Companion to the JSON schema at
//! `tools/wtpool/schemas/worktree-lease.v1.json`. This
//! module implements the reader / writer / validator side of the
//! contract; sister branch `codex-worker-allowedpaths-hook` consumes
//! the validators in PreToolUse / PostToolUse hooks.
//!
//! Design background: see
//! `docs/research/2026-04-29-codex-mcp-worker-feasibility.md` §4 for
//! the four-mechanism enforcement strategy (path-glob hook, test-cmd
//! authorization, worktree boundary `cwd`, lease audit). This module
//! lands the data structure + validators for steps 1 / 2 / 4. Step 3
//! lives in the sister Codex shim's `--cd` plumbing.
//!
//! Why this lives in `wtpool` and not a standalone crate:
//! a lease is intrinsic to a worktree's lifecycle — the slot the
//! worker writes inside, the branch it commits on, and the merge
//! authority that gates landing into main are all worktree-scoped
//! concepts the existing crate already owns.
//!
//! ## Glob semantics
//!
//! Patterns use a small subset of standard glob syntax:
//!
//! - `*` matches zero or more characters within a single path segment
//!   (does not cross `/`).
//! - `**` matches zero or more full path segments. Standalone `**`
//!   matches everything; `src/**` matches every descendant under
//!   `src/`. Adjacent `**/foo` matches `foo` at any depth.
//! - Any other character is matched literally.
//!
//! Question marks, character classes, and brace expansion are NOT
//! supported. The matcher errs strictly toward "if you didn't list
//! this pattern, the path doesn't match" — security-via-simplicity.
//!
//! ## Forbidden vs allowed precedence
//!
//! When a path matches both `allowed_paths` and `forbidden_paths`,
//! `forbidden_paths` wins. This makes it safe to write
//! `allowed_paths: ["src/**"]` + `forbidden_paths: ["src/orchestration/**"]`
//! without worrying about overlap.
//!
//! ## Test command match
//!
//! Test commands are matched exactly — no glob, no substring, no
//! prefix. This is deliberate. A "prefix match" rule lets a worker
//! sneak `cargo test -p X --release; rm -rf /` past the gate; an
//! exact match makes the lease's `test_commands` array the literal
//! whitelist of bash invocations.

#![allow(clippy::result_large_err)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Canonical filename a lease lives under inside its worktree.
/// Hooks read `<worktree>/.wt-lease.json` to recover the contract.
pub const LEASE_FILENAME: &str = ".wt-lease.json";

/// The single supported schema version. Bumps mean a separate
/// `worktree-lease.v2.rs` module + schema file.
pub const SCHEMA_VERSION: u32 = 1;

/// Worker tier the lease is issued to. Mirrors the enum in
/// `worktree-lease.v1.json::worker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Worker {
    /// Claude Opus model.
    ClaudeOpus,
    /// Claude Sonnet model.
    ClaudeSonnet,
    /// Claude Haiku model.
    ClaudeHaiku,
    /// OpenAI Codex GPT-5.x model.
    Codex,
    /// Manually-driven dispatch (operator at the keyboard).
    Human,
}

/// In-memory representation of a lease. Round-trips through JSON via
/// serde and validates against the v1 schema's required fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeLease {
    /// Schema version. Must be `SCHEMA_VERSION` for this module to
    /// accept the lease. Future v2+ files will live in a separate
    /// module.
    pub schema_version: u32,
    /// Stable identifier for the dispatch. Hooks use this to
    /// correlate diff vs lease.
    pub task_id: String,
    /// Worker tier this lease is issued to.
    pub worker: Worker,
    /// Absolute path to the worktree the lease applies to.
    pub worktree: PathBuf,
    /// Branch the worker is committing on.
    pub branch: String,
    /// Glob patterns the worker may write under (repo-relative).
    pub allowed_paths: Vec<String>,
    /// Glob patterns the worker must not write under. Takes
    /// precedence over `allowed_paths` on overlap.
    pub forbidden_paths: Vec<String>,
    /// Exact-match shell commands the worker is allowed to run.
    pub test_commands: Vec<String>,
    /// Reviewer voice authorized to PROCEED the merge.
    pub merge_authority: String,
    /// RFC 3339 / ISO 8601 timestamp lease was issued.
    pub created_at: String,
    /// Optional RFC 3339 soft-expiry timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Optional parent dispatch correlation id (reserved; not yet
    /// consumed by any hook).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
}

/// Errors surfaced by the lease reader / writer / validators.
///
/// Stable wire strings under `kind()` so MCP callers can branch on
/// `error_kind` without parsing the human message.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    /// File on disk could not be read or written.
    #[error("io: {path}: {source}")]
    Io {
        /// Path that produced the error.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// JSON parse failed before serde could even map fields.
    #[error("invalid_json: {path}: {source}")]
    InvalidJson {
        /// Path the bytes were read from. Empty when validating an
        /// in-memory string.
        path: PathBuf,
        /// Serde error.
        #[source]
        source: serde_json::Error,
    },
    /// Required field missing or has the wrong type. The schema
    /// reader emits this with a human-readable field name (e.g.
    /// `task_id`).
    #[error("missing_field: {field}")]
    MissingField {
        /// Field name as it appears in the JSON.
        field: String,
    },
    /// Schema version on the file doesn't match `SCHEMA_VERSION`.
    #[error("unsupported_schema_version: got {got}, want {want}")]
    UnsupportedSchemaVersion {
        /// Version actually present in the file.
        got: u32,
        /// Version this binary supports.
        want: u32,
    },
    /// Lease points at a path that doesn't exist on disk. Surfaced
    /// by the writer when `validate_worktree_exists` is on, and by
    /// the read path when the consumer asks for filesystem
    /// validation.
    #[error("worktree_not_found: {path}")]
    WorktreeNotFound {
        /// The missing path.
        path: PathBuf,
    },
    /// `created_at` or `expires_at` is not parseable as RFC 3339.
    #[error("invalid_timestamp: {field}={value}")]
    InvalidTimestamp {
        /// Field name (e.g. `created_at`).
        field: String,
        /// The raw value that failed to parse.
        value: String,
    },
    /// `task_id` doesn't match the schema's pattern.
    #[error("invalid_task_id: {value}")]
    InvalidTaskId {
        /// The rejected value.
        value: String,
    },
}

impl LeaseError {
    /// Stable wire string suitable for MCP `error_kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            LeaseError::Io { .. } => "io",
            LeaseError::InvalidJson { .. } => "invalid_json",
            LeaseError::MissingField { .. } => "missing_field",
            LeaseError::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
            LeaseError::WorktreeNotFound { .. } => "worktree_not_found",
            LeaseError::InvalidTimestamp { .. } => "invalid_timestamp",
            LeaseError::InvalidTaskId { .. } => "invalid_task_id",
        }
    }
}

impl WorktreeLease {
    /// Read + validate a lease from the canonical
    /// `<worktree>/.wt-lease.json` filename.
    pub fn read_from_worktree(worktree: &Path) -> Result<Self, LeaseError> {
        Self::read_from_file(&worktree.join(LEASE_FILENAME))
    }

    /// Read + validate a lease from an arbitrary path.
    pub fn read_from_file(path: &Path) -> Result<Self, LeaseError> {
        let bytes = fs::read(path).map_err(|source| LeaseError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json_bytes(&bytes, Some(path.to_path_buf()))
    }

    /// Parse + validate a lease from raw JSON bytes. `provenance`
    /// labels the source path in error messages; pass `None` when
    /// validating an in-memory string.
    pub fn from_json_bytes(bytes: &[u8], provenance: Option<PathBuf>) -> Result<Self, LeaseError> {
        let path = provenance.unwrap_or_default();
        let lease: WorktreeLease = serde_json::from_slice(bytes).map_err(|source| {
            // Map serde's "missing field X" error to the structured
            // variant so MCP callers can branch on error_kind.
            let msg = source.to_string();
            if let Some(field) = parse_serde_missing_field(&msg) {
                LeaseError::MissingField { field }
            } else {
                LeaseError::InvalidJson { path, source }
            }
        })?;
        lease.validate_self()?;
        Ok(lease)
    }

    /// Internal post-deserialize sanity. Catches things serde
    /// couldn't (e.g. schema_version mismatch, malformed task_id,
    /// unparseable timestamps).
    fn validate_self(&self) -> Result<(), LeaseError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(LeaseError::UnsupportedSchemaVersion {
                got: self.schema_version,
                want: SCHEMA_VERSION,
            });
        }
        if !is_valid_task_id(&self.task_id) {
            return Err(LeaseError::InvalidTaskId {
                value: self.task_id.clone(),
            });
        }
        if !is_valid_rfc3339(&self.created_at) {
            return Err(LeaseError::InvalidTimestamp {
                field: "created_at".into(),
                value: self.created_at.clone(),
            });
        }
        if let Some(exp) = &self.expires_at {
            if !is_valid_rfc3339(exp) {
                return Err(LeaseError::InvalidTimestamp {
                    field: "expires_at".into(),
                    value: exp.clone(),
                });
            }
        }
        Ok(())
    }

    /// Atomically write the lease to `path`. Writes to a
    /// `tempfile::NamedTempFile` in the same directory, fsyncs, then
    /// renames into place. Refuses to write if
    /// `validate_worktree_exists=true` and the lease's `worktree`
    /// field doesn't point at an existing directory.
    ///
    /// Tempfile uniqueness comes from `tempfile::NamedTempFile::new_in`
    /// (OS-managed entropy via `getrandom`), not a hand-rolled
    /// `process_id ^ clock_subsec` scheme — torvalds 2026-04-29
    /// flagged the prior approach as collision-prone under simultaneous
    /// emits. The same-directory placement keeps the rename atomic on
    /// the final-name's filesystem.
    pub fn write(&self, path: &Path, validate_worktree_exists: bool) -> Result<(), LeaseError> {
        if validate_worktree_exists && !self.worktree.is_dir() {
            return Err(LeaseError::WorktreeNotFound {
                path: self.worktree.clone(),
            });
        }
        self.validate_self()?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let json = serde_json::to_vec_pretty(self).map_err(|source| LeaseError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".lease-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|source| LeaseError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        let tmp_path = tmp.path().to_path_buf();
        {
            let f = tmp.as_file_mut();
            f.write_all(&json).map_err(|source| LeaseError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            f.write_all(b"\n").map_err(|source| LeaseError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| LeaseError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        }
        // `persist` does the rename and consumes the NamedTempFile so
        // the Drop impl doesn't try to unlink the (now-renamed) path.
        tmp.persist(path).map_err(|e| LeaseError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        Ok(())
    }

    /// Write the lease to the canonical
    /// `<worktree>/.wt-lease.json` location.
    pub fn write_to_worktree(&self) -> Result<(), LeaseError> {
        let path = self.worktree.join(LEASE_FILENAME);
        self.write(&path, true)
    }

    /// True iff `path` (repo-relative) is permitted by this lease:
    /// matches at least one `allowed_paths` entry AND no
    /// `forbidden_paths` entry.
    ///
    /// `pub` (vs `pub(crate)`) is load-bearing per Rule 12: the
    /// `wtpool` `[[bin]]` target is a separate
    /// compilation unit from the library, and `main.rs` consumes
    /// this method through the library's public surface. Same for
    /// `matches_test_command` + `matches_cwd` below.
    pub fn matches_path(&self, path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        if self.forbidden_paths.iter().any(|pat| glob_match(pat, path)) {
            return false;
        }
        self.allowed_paths.iter().any(|pat| glob_match(pat, path))
    }

    /// True iff `cmd` is exact-equal to one of the lease's
    /// `test_commands`. No prefix match, no substring match.
    pub fn matches_test_command(&self, cmd: &str) -> bool {
        self.test_commands.iter().any(|c| c == cmd)
    }

    /// True iff `cwd` (after symlink resolution) lies inside the
    /// lease's `worktree` subtree. Used by hooks to fence a tool call's
    /// working directory against the lease boundary. Both sides are
    /// resolved via `std::fs::canonicalize` before the prefix
    /// comparison so a symlink pointing outside the worktree fails the
    /// check (TOCTOU mitigation per feasibility doc §4.3). Returns
    /// false when either canonicalisation fails — fail-closed when the
    /// path doesn't exist or the operator can't see it.
    pub fn matches_cwd(&self, cwd: &Path) -> bool {
        let Ok(cwd_canon) = fs::canonicalize(cwd) else {
            return false;
        };
        let Ok(wt_canon) = fs::canonicalize(&self.worktree) else {
            return false;
        };
        cwd_canon.starts_with(&wt_canon)
    }

    /// True iff a `now` timestamp (RFC 3339) is past `expires_at`.
    /// Returns false when no expiry is set or when either timestamp
    /// fails to parse — soft-expiry is intentionally tolerant.
    pub fn is_expired(&self, now: &str) -> bool {
        let Some(exp) = &self.expires_at else {
            return false;
        };
        let Some(now_secs) = rfc3339_to_unix(now) else {
            return false;
        };
        let Some(exp_secs) = rfc3339_to_unix(exp) else {
            return false;
        };
        now_secs > exp_secs
    }
}

/// Extract the "missing field `X`" name from a serde error string.
/// Serde phrases these consistently across versions:
/// `missing field \`task_id\` at line 4 column 3`.
fn parse_serde_missing_field(msg: &str) -> Option<String> {
    let prefix = "missing field `";
    let i = msg.find(prefix)?;
    let rest = &msg[i + prefix.len()..];
    let j = rest.find('`')?;
    Some(rest[..j].to_string())
}

/// `task_id` validator mirroring the schema's regex
/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`.
fn is_valid_task_id(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Glob match a single repo-relative `path` against a single
/// pattern. Supports `*` (single-segment wildcard) and `**`
/// (cross-segment wildcard). See module docs for full semantics.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    glob_match_segments(&pat_segments, &path_segments)
}

fn glob_match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            // Try `**` matching zero, one, two, ... path segments.
            // `glob_match_segments(rest_pat, path)` covers the
            // "match zero" case; if path is empty and pattern after
            // `**` is also empty, we land on the (None, None) base
            // case which is true (so `src/**` matches `src/`).
            for i in 0..=path.len() {
                if glob_match_segments(&pattern[1..], &path[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(_), None) => false,
        (Some(p_seg), Some(path_seg)) => {
            if !glob_segment_match(p_seg, path_seg) {
                return false;
            }
            glob_match_segments(&pattern[1..], &path[1..])
        }
    }
}

/// Single-segment glob match: `*` matches any run of non-`/`
/// characters, all other characters match literally. No `?`, no
/// `[...]`, no escapes.
fn glob_segment_match(pattern: &str, segment: &str) -> bool {
    let p = pattern.as_bytes();
    let s = segment.as_bytes();
    glob_seg_recurse(p, 0, s, 0)
}

fn glob_seg_recurse(pat: &[u8], pi: usize, seg: &[u8], si: usize) -> bool {
    if pi == pat.len() {
        return si == seg.len();
    }
    if pat[pi] == b'*' {
        // `*` matches zero or more characters in the segment.
        if glob_seg_recurse(pat, pi + 1, seg, si) {
            return true;
        }
        if si < seg.len() {
            return glob_seg_recurse(pat, pi, seg, si + 1);
        }
        return false;
    }
    if si == seg.len() {
        return false;
    }
    if pat[pi] != seg[si] {
        return false;
    }
    glob_seg_recurse(pat, pi + 1, seg, si + 1)
}

/// Minimal RFC 3339 validator. Accepts forms:
/// `YYYY-MM-DDTHH:MM:SSZ` and `YYYY-MM-DDTHH:MM:SS+HH:MM` (or `-HH:MM`).
/// Sub-second precision (`.123`) is accepted between seconds and the
/// timezone marker. Strict enough to catch human typos, lenient
/// enough that any standard ISO 8601 producer round-trips.
fn is_valid_rfc3339(s: &str) -> bool {
    rfc3339_to_unix(s).is_some()
}

/// Parse RFC 3339 to Unix seconds. Returns `None` on any malformed
/// input. Implementation is hand-rolled to avoid pulling in `time` or
/// `chrono` for one timestamp parser. Leap seconds (`60` second) are
/// rejected; the rest of standard RFC 3339 is accepted.
///
/// **Lossy round-trip on fractional seconds.** This parser accepts
/// `.NNN` fractional seconds (drops them after validating digits) and
/// `unix_to_rfc3339` always emits second precision. So
/// `2026-04-29T15:30:45.123Z` round-trips to
/// `2026-04-29T15:30:45Z`. Callers that need millisecond fidelity
/// must store + emit their own format. The use case here is lease
/// `created_at` / `expires_at`, which are second-grained by design.
fn rfc3339_to_unix(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let bytes = s.as_bytes();
    let year: i64 = parse_digits(&bytes[0..4])?;
    if bytes[4] != b'-' {
        return None;
    }
    let month: u32 = parse_digits(&bytes[5..7])?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: u32 = parse_digits(&bytes[8..10])?;
    if bytes[10] != b'T' && bytes[10] != b't' {
        return None;
    }
    let hour: u32 = parse_digits(&bytes[11..13])?;
    if bytes[13] != b':' {
        return None;
    }
    let minute: u32 = parse_digits(&bytes[14..16])?;
    if bytes[16] != b':' {
        return None;
    }
    let second: u32 = parse_digits(&bytes[17..19])?;
    let mut idx = 19;
    // Optional fractional seconds.
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start {
            return None;
        }
    }
    if idx >= bytes.len() {
        return None;
    }
    // Timezone: Z, +HH:MM, or -HH:MM.
    let tz_offset_secs: i64 = match bytes[idx] {
        b'Z' | b'z' => {
            if idx + 1 != bytes.len() {
                return None;
            }
            0
        }
        sign @ (b'+' | b'-') => {
            if idx + 6 != bytes.len() || bytes[idx + 3] != b':' {
                return None;
            }
            let oh: i64 = parse_digits(&bytes[idx + 1..idx + 3])?;
            let om: i64 = parse_digits(&bytes[idx + 4..idx + 6])?;
            let total = oh * 3600 + om * 60;
            if sign == b'+' {
                total
            } else {
                -total
            }
        }
        _ => return None,
    };

    if !(1..=12).contains(&month) {
        return None;
    }
    if !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let utc = ymd_hms_to_unix(year, month, day, hour, minute, second);
    Some(utc - tz_offset_secs)
}

fn parse_digits<T>(b: &[u8]) -> Option<T>
where
    T: std::str::FromStr,
{
    let s = std::str::from_utf8(b).ok()?;
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Convert a UTC date+time to Unix seconds. Uses Howard Hinnant's
/// civil-from-days algorithm so we don't pull in `time`.
fn ymd_hms_to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    // days_from_civil — Hinnant 2013, public domain.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64; // [0, 399]
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days_since_epoch = era * 146097 + doe as i64 - 719468;
    days_since_epoch * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
}

/// Format the current UTC moment as RFC 3339 (`...Z`) with second
/// precision. Used by lease emitters that don't supply `created_at`
/// explicitly.
pub fn now_rfc3339() -> String {
    let secs_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unix_to_rfc3339(secs_since_epoch)
}

fn unix_to_rfc3339(unix_secs: i64) -> String {
    // Inverse of `ymd_hms_to_unix`. Use Hinnant civil_from_days.
    let days = unix_secs.div_euclid(86400);
    let secs_of_day = unix_secs.rem_euclid(86400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Builder helpers for emitting a fresh lease without exposing every
/// field at every callsite. Used by the CLI `lease emit` subcommand
/// and the MCP `worktree_lease_emit` RPC.
#[derive(Debug, Clone, Default)]
pub struct LeaseEmitArgs {
    /// `task_id` field. Required.
    pub task_id: String,
    /// `worker` enum. Required.
    pub worker: Option<Worker>,
    /// Worktree path. Required.
    pub worktree: PathBuf,
    /// Branch name. Required.
    pub branch: String,
    /// Glob list. Empty => "no writes allowed".
    pub allowed_paths: Vec<String>,
    /// Glob list of forbidden paths.
    pub forbidden_paths: Vec<String>,
    /// Test command whitelist.
    pub test_commands: Vec<String>,
    /// Defaults to "torvalds-review-agent" when None.
    pub merge_authority: Option<String>,
    /// Optional soft-expiry RFC 3339 stamp.
    pub expires_at: Option<String>,
    /// Optional parent task correlation.
    pub parent_task_id: Option<String>,
}

impl LeaseEmitArgs {
    /// Materialise into a [`WorktreeLease`]. Defaults applied here:
    /// `created_at` = now, `merge_authority` = `torvalds-review-agent`,
    /// `worker` = error if unset (no implicit worker).
    pub fn into_lease(self) -> Result<WorktreeLease, LeaseError> {
        let worker = self.worker.ok_or(LeaseError::MissingField {
            field: "worker".into(),
        })?;
        if self.task_id.is_empty() {
            return Err(LeaseError::MissingField {
                field: "task_id".into(),
            });
        }
        if self.branch.is_empty() {
            return Err(LeaseError::MissingField {
                field: "branch".into(),
            });
        }
        let lease = WorktreeLease {
            schema_version: SCHEMA_VERSION,
            task_id: self.task_id,
            worker,
            worktree: self.worktree,
            branch: self.branch,
            allowed_paths: self.allowed_paths,
            forbidden_paths: self.forbidden_paths,
            test_commands: self.test_commands,
            merge_authority: self
                .merge_authority
                .unwrap_or_else(|| "torvalds-review-agent".to_string()),
            created_at: now_rfc3339(),
            expires_at: self.expires_at,
            parent_task_id: self.parent_task_id,
        };
        lease.validate_self()?;
        Ok(lease)
    }
}

/// MCP-side error mapper: turn a `LeaseError` into a `(message,
/// error_kind)` pair the JSON-RPC error response can carry.
pub fn lease_error_to_mcp(e: &LeaseError) -> (String, Value) {
    (e.to_string(), json!({ "error_kind": e.kind() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn minimal_lease() -> WorktreeLease {
        WorktreeLease {
            schema_version: SCHEMA_VERSION,
            task_id: "physics-bridge-017".into(),
            worker: Worker::Codex,
            worktree: PathBuf::from("/tmp/wtpool/wt-04"),
            branch: "physics-bridge-017".into(),
            allowed_paths: vec!["src/physics/**".into(), "tests/physics/**".into()],
            forbidden_paths: vec![
                "CLAUDE.md".into(),
                ".claude/**".into(),
                "tools/guardian/**".into(),
            ],
            test_commands: vec!["cargo test -p mycrate-physics".into()],
            merge_authority: "torvalds-review-agent".into(),
            created_at: "2026-04-29T15:00:00Z".into(),
            expires_at: None,
            parent_task_id: None,
        }
    }

    #[test]
    fn minimal_lease_round_trips_through_json() {
        let lease = minimal_lease();
        let s = serde_json::to_string(&lease).unwrap();
        let back = WorktreeLease::from_json_bytes(s.as_bytes(), None).unwrap();
        assert_eq!(lease, back);
    }

    #[test]
    fn missing_task_id_surfaces_named_error() {
        let mut v = serde_json::to_value(minimal_lease()).unwrap();
        v.as_object_mut().unwrap().remove("task_id");
        let err = WorktreeLease::from_json_bytes(v.to_string().as_bytes(), None).unwrap_err();
        match err {
            LeaseError::MissingField { field } => assert_eq!(field, "task_id"),
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_rejected_by_deny_unknown_fields() {
        let mut v = serde_json::to_value(minimal_lease()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("rogue_field".into(), json!("anything"));
        let err = WorktreeLease::from_json_bytes(v.to_string().as_bytes(), None).unwrap_err();
        // serde_json reports unknown fields as `unknown field
        // \`rogue_field\`` — surfaced as InvalidJson because the
        // missing-field parser doesn't match.
        assert!(matches!(err, LeaseError::InvalidJson { .. }), "{err:?}");
    }

    #[test]
    fn schema_version_mismatch_rejected() {
        let mut lease = minimal_lease();
        lease.schema_version = 999;
        let v = serde_json::to_value(&lease).unwrap();
        let err = WorktreeLease::from_json_bytes(v.to_string().as_bytes(), None).unwrap_err();
        match err {
            LeaseError::UnsupportedSchemaVersion { got, want } => {
                assert_eq!(got, 999);
                assert_eq!(want, SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn invalid_task_id_rejected() {
        let mut lease = minimal_lease();
        lease.task_id = "has spaces".into();
        let v = serde_json::to_value(&lease).unwrap();
        let err = WorktreeLease::from_json_bytes(v.to_string().as_bytes(), None).unwrap_err();
        assert!(matches!(err, LeaseError::InvalidTaskId { .. }), "{err:?}");
    }

    #[test]
    fn invalid_created_at_rejected() {
        let mut lease = minimal_lease();
        lease.created_at = "not-a-timestamp".into();
        let v = serde_json::to_value(&lease).unwrap();
        let err = WorktreeLease::from_json_bytes(v.to_string().as_bytes(), None).unwrap_err();
        assert!(
            matches!(err, LeaseError::InvalidTimestamp { ref field, .. } if field == "created_at"),
            "{err:?}"
        );
    }

    #[test]
    fn matches_path_allowed_glob_hits() {
        let lease = minimal_lease();
        assert!(lease.matches_path("src/physics/foo.rs"));
        assert!(lease.matches_path("src/physics/sub/bar.rs"));
        assert!(lease.matches_path("tests/physics/main.rs"));
    }

    #[test]
    fn matches_path_unlisted_path_rejected() {
        let lease = minimal_lease();
        assert!(!lease.matches_path("src/audio/foo.rs"));
        assert!(!lease.matches_path("Cargo.toml"));
    }

    #[test]
    fn matches_path_forbidden_takes_precedence_over_allowed() {
        // Build an overlap: allowed `src/**`, forbidden
        // `src/physics/**`. `src/physics/foo.rs` matches both —
        // forbidden must win.
        let lease = WorktreeLease {
            allowed_paths: vec!["src/**".into()],
            forbidden_paths: vec!["src/physics/**".into()],
            ..minimal_lease()
        };
        assert!(lease.matches_path("src/audio/foo.rs"));
        assert!(!lease.matches_path("src/physics/foo.rs"));
    }

    #[test]
    fn matches_path_empty_path_rejected() {
        let lease = minimal_lease();
        assert!(!lease.matches_path(""));
    }

    #[test]
    fn matches_test_command_exact_only() {
        let lease = WorktreeLease {
            test_commands: vec!["cargo test -p mycrate-physics".into()],
            ..minimal_lease()
        };
        assert!(lease.matches_test_command("cargo test -p mycrate-physics"));
        // Mutation-equivalent: changing exact-match to substring or
        // prefix match would let `--release` past. This test pins the
        // semantics.
        assert!(!lease.matches_test_command("cargo test -p mycrate-physics --release"));
        assert!(!lease.matches_test_command("cargo test -p mycrate-physic"));
        assert!(!lease.matches_test_command("rm -rf /"));
    }

    #[test]
    fn is_expired_no_expiry_returns_false() {
        let lease = minimal_lease();
        assert!(!lease.is_expired("2099-01-01T00:00:00Z"));
    }

    #[test]
    fn is_expired_past_expiry_returns_true() {
        let lease = WorktreeLease {
            expires_at: Some("2026-04-29T16:00:00Z".into()),
            ..minimal_lease()
        };
        assert!(lease.is_expired("2026-04-29T17:00:00Z"));
        assert!(!lease.is_expired("2026-04-29T15:30:00Z"));
    }

    #[test]
    fn glob_match_double_star_crosses_segments() {
        assert!(glob_match("src/**", "src/a/b/c.rs"));
        assert!(glob_match("src/**", "src/foo.rs"));
        assert!(glob_match("**", "anything/here"));
        assert!(glob_match("**/foo", "a/b/foo"));
        assert!(glob_match("**/foo", "foo"));
        assert!(!glob_match("src/**", "tests/foo.rs"));
    }

    #[test]
    fn glob_match_single_star_within_segment() {
        assert!(glob_match("src/*.rs", "src/foo.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/foo.rs"));
        assert!(glob_match("*.md", "README.md"));
        assert!(!glob_match("*.md", "sub/README.md"));
    }

    #[test]
    fn glob_match_literal_segment() {
        assert!(glob_match("CLAUDE.md", "CLAUDE.md"));
        assert!(!glob_match("CLAUDE.md", "claude.md"));
        assert!(!glob_match("CLAUDE.md", "src/CLAUDE.md"));
    }

    #[test]
    fn glob_match_dot_claude_pattern_matches_subdir() {
        assert!(glob_match(".claude/**", ".claude/hooks/foo.sh"));
        assert!(glob_match(".claude/**", ".claude/hooks"));
        assert!(!glob_match(".claude/**", "tools/foo.sh"));
    }

    #[test]
    fn write_round_trips_via_tempdir() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        // Worktree the lease points at must exist when we write.
        let wt = tmp.path().join("worktree-x");
        fs::create_dir_all(&wt).unwrap();
        let lease = WorktreeLease {
            worktree: wt.clone(),
            ..minimal_lease()
        };
        let lease_path = wt.join(LEASE_FILENAME);
        lease.write(&lease_path, true).unwrap();
        let back = WorktreeLease::read_from_file(&lease_path).unwrap();
        assert_eq!(lease, back);
    }

    #[test]
    fn write_concurrent_emits_no_corruption() {
        // Race regression: torvalds 2026-04-29 flagged the prior
        // hand-rolled `process_id ^ clock_subsec` tempfile naming as
        // collision-prone under simultaneous emits. With the
        // tempfile::NamedTempFile fix, N concurrent writers against
        // the same target should each succeed (the last rename wins,
        // but no writer corrupts another's tempfile bytes).
        use std::sync::Arc;
        use std::thread;
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("worktree-race");
        std::fs::create_dir_all(&wt).unwrap();
        let lease = Arc::new(WorktreeLease {
            worktree: wt.clone(),
            ..minimal_lease()
        });
        let lease_path = wt.join(LEASE_FILENAME);
        let mut handles = Vec::new();
        for _ in 0..16 {
            let lease = Arc::clone(&lease);
            let path = lease_path.clone();
            handles.push(thread::spawn(move || lease.write(&path, true)));
        }
        for h in handles {
            // Every writer must succeed. Mutation: revert to the
            // hand-rolled scheme and run with --test-threads=16; under
            // sufficient contention you'd see EEXIST or partial-write
            // corruption surface on read-back.
            h.join().unwrap().unwrap();
        }
        // Final state must be a valid, parseable lease (not a
        // half-written tempfile that leaked through).
        let back = WorktreeLease::read_from_file(&lease_path).unwrap();
        assert_eq!(back.task_id, "physics-bridge-017");
    }

    #[test]
    fn write_rejects_nonexistent_worktree_when_validation_on() {
        let lease = WorktreeLease {
            worktree: PathBuf::from("/definitely/does/not/exist/wt-99"),
            ..minimal_lease()
        };
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(LEASE_FILENAME);
        let err = lease.write(&target, true).unwrap_err();
        assert!(
            matches!(err, LeaseError::WorktreeNotFound { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn read_from_file_missing_path_returns_io_error() {
        let p = PathBuf::from("/tmp/nonexistent-lease-zzz.json");
        let err = WorktreeLease::read_from_file(&p).unwrap_err();
        assert!(matches!(err, LeaseError::Io { .. }), "{err:?}");
    }

    #[test]
    fn rfc3339_round_trip() {
        let s = "2026-04-29T15:30:45Z";
        let unix = rfc3339_to_unix(s).unwrap();
        let back = unix_to_rfc3339(unix);
        assert_eq!(back, s);
    }

    #[test]
    fn rfc3339_offset_parses() {
        // 2026-04-29T15:00:00Z == 2026-04-29T17:00:00+02:00.
        let z = rfc3339_to_unix("2026-04-29T15:00:00Z").unwrap();
        let off = rfc3339_to_unix("2026-04-29T17:00:00+02:00").unwrap();
        assert_eq!(z, off);
    }

    #[test]
    fn rfc3339_rejects_garbage() {
        assert!(rfc3339_to_unix("not-a-date").is_none());
        assert!(rfc3339_to_unix("2026-13-01T00:00:00Z").is_none()); // bad month
        assert!(rfc3339_to_unix("2026-02-30T00:00:00Z").is_none()); // bad day
        assert!(rfc3339_to_unix("2026-04-29T25:00:00Z").is_none()); // bad hour
    }

    #[test]
    fn worker_enum_round_trips_kebab_case() {
        let workers = [
            Worker::ClaudeOpus,
            Worker::ClaudeSonnet,
            Worker::ClaudeHaiku,
            Worker::Codex,
            Worker::Human,
        ];
        let mut wires = HashSet::new();
        for w in workers {
            let s = serde_json::to_string(&w).unwrap();
            wires.insert(s.clone());
            let back: Worker = serde_json::from_str(&s).unwrap();
            assert_eq!(w, back);
        }
        assert!(wires.contains("\"claude-opus\""));
        assert!(wires.contains("\"codex\""));
    }

    #[test]
    fn lease_emit_args_defaults_merge_authority() {
        let args = LeaseEmitArgs {
            task_id: "test-task-1".into(),
            worker: Some(Worker::ClaudeHaiku),
            worktree: PathBuf::from("/tmp/x"),
            branch: "test-task-1".into(),
            allowed_paths: vec!["src/**".into()],
            ..Default::default()
        };
        let lease = args.into_lease().unwrap();
        assert_eq!(lease.merge_authority, "torvalds-review-agent");
    }

    #[test]
    fn matches_cwd_inside_worktree_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt-x");
        std::fs::create_dir_all(wt.join("src/sub")).unwrap();
        let lease = WorktreeLease {
            worktree: wt.clone(),
            ..minimal_lease()
        };
        assert!(lease.matches_cwd(&wt));
        assert!(lease.matches_cwd(&wt.join("src")));
        assert!(lease.matches_cwd(&wt.join("src/sub")));
    }

    #[test]
    fn matches_cwd_outside_worktree_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt-y");
        let outside = tmp.path().join("not-wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let lease = WorktreeLease {
            worktree: wt.clone(),
            ..minimal_lease()
        };
        assert!(!lease.matches_cwd(&outside));
    }

    #[test]
    fn matches_cwd_symlink_pointing_outside_fails() {
        // Symlink TOCTOU regression-pin (feasibility doc §4.3): if the
        // hook compared raw textual paths, a symlink inside the
        // worktree pointing outside would let the tool call escape.
        // canonicalize() resolves the link first, so the prefix check
        // compares the resolved target against the worktree root.
        // Mutation: drop canonicalize() and the test starts passing
        // when it should fail.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let tmp = tempfile::tempdir().unwrap();
            let wt = tmp.path().join("wt-sym");
            let outside = tmp.path().join("escape-target");
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            let link = wt.join("escape-link");
            symlink(&outside, &link).unwrap();
            let lease = WorktreeLease {
                worktree: wt.clone(),
                ..minimal_lease()
            };
            assert!(
                !lease.matches_cwd(&link),
                "symlink that resolves outside the worktree must NOT match"
            );
        }
    }

    #[test]
    fn matches_cwd_nonexistent_path_fails_closed() {
        let lease = minimal_lease();
        assert!(!lease.matches_cwd(Path::new("/definitely/does/not/exist/zzz")));
    }

    #[test]
    fn lease_emit_args_missing_worker_errors() {
        let args = LeaseEmitArgs {
            task_id: "test-task-1".into(),
            worker: None,
            worktree: PathBuf::from("/tmp/x"),
            branch: "test-task-1".into(),
            ..Default::default()
        };
        let err = args.into_lease().unwrap_err();
        assert!(
            matches!(err, LeaseError::MissingField { ref field } if field == "worker"),
            "{err:?}"
        );
    }
}
