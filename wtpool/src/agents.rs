//! `agent_inflight_summary` backend.
//!
//! Cross-references two telemetry streams that a typical CLI agent
//! harness emits at every subagent tool invocation:
//!
//! 1. **Per-task `*.output` mtimes** under
//!    `/tmp/<prefix><uid>/-workspace/<session-id>/tasks/<task-id>.output`,
//!    where `<prefix>` defaults to `claude-` (matching the Claude
//!    Code CLI layout) and is overridable via the
//!    `WTPOOL_TASK_DIR_PREFIX` env var. The mtime advances on every
//!    JSONL append by the harness's transcript writer; an mtime
//!    older than the staleness cutoff means the agent has stopped
//!    emitting.
//! 2. **Heartbeat sentinels** at `/tmp/agent-<task-id>.progress`,
//!    written by an external heartbeat hook. Single line, format
//!    `"<ISO-ts> <tool> <truncated-args>"`. Not all sessions have
//!    the hook wired — heartbeats are best-effort, the per-task
//!    mtimes remain authoritative.
//!
//! Output groups inflight tasks by worktree path. Worktree
//! association is best-effort substring match: an `args` line in
//! `agent-<id>.progress` often contains the worktree path when the
//! agent ran a `cd /tmp/wtpool/<wt>` command — we attribute the
//! agent to that worktree. Agents not associable with a worktree are
//! bucketed under `<unattributed>`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Default staleness cutoff (matches `tools/subagent-liveness.sh`).
pub const DEFAULT_STALE_MINUTES: u64 = 5;

/// Single inflight-or-recently-stopped task observation.
#[derive(Debug)]
struct AgentObservation {
    task_id: String,
    last_mtime: SystemTime,
    /// First worktree path mentioned in either the `*.output` symlink
    /// target or the heartbeat sentinel's args line, if any.
    associated_worktree: Option<PathBuf>,
    /// Last tool invocation extracted from heartbeat sentinel, if any.
    last_tool: Option<String>,
}

/// `agent_inflight_summary` body.
///
/// Returns `{worktrees: [{path, branch, last_agent_tool_ts, agent_ids,
/// stale}]}` per spec §1.2. `path` is `<unattributed>` for agents we
/// could not associate with a worktree by string match.
pub fn agent_inflight_summary(
    repo_root: &Path,
    stale_minutes: u64,
    known_worktrees: &[PathBuf],
) -> Result<Value, String> {
    let observations = scan_observations()?;
    let now = SystemTime::now();
    let cutoff = std::time::Duration::from_secs(stale_minutes * 60);

    // Group by associated worktree path; empty group lives under the
    // `<unattributed>` synthetic key.
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<&AgentObservation>> = BTreeMap::new();
    for obs in &observations {
        let key = match &obs.associated_worktree {
            Some(p) => p.display().to_string(),
            None => match associate_via_known(obs, known_worktrees) {
                Some(p) => p.display().to_string(),
                None => "<unattributed>".to_string(),
            },
        };
        grouped.entry(key).or_default().push(obs);
    }

    let mut entries = Vec::new();
    for (path_str, obs_list) in &grouped {
        let mut latest_mtime: Option<SystemTime> = None;
        let mut agent_ids = Vec::new();
        let mut stale = true;
        for o in obs_list {
            agent_ids.push(json!({
                "task_id": o.task_id,
                "last_tool": o.last_tool.clone().unwrap_or_default(),
                "last_mtime": iso8601(o.last_mtime),
                "stale": now
                    .duration_since(o.last_mtime)
                    .map(|d| d > cutoff)
                    .unwrap_or(true),
            }));
            if !now
                .duration_since(o.last_mtime)
                .map(|d| d > cutoff)
                .unwrap_or(true)
            {
                stale = false;
            }
            if latest_mtime.is_none_or(|lm| o.last_mtime > lm) {
                latest_mtime = Some(o.last_mtime);
            }
        }
        // Attempt to resolve the worktree's branch via git, but
        // silently fall back when path is `<unattributed>` or the path
        // is not a real worktree.
        let branch = if path_str == "<unattributed>" {
            None
        } else {
            // open_repo + head() chained — head's Reference borrows the
            // repo, so we must hold the repo alive across the
            // shorthand read. Inline rather than try to chain through
            // and_then.
            crate::git::open_repo(Path::new(path_str))
                .ok()
                .and_then(|r| {
                    r.head()
                        .ok()
                        .and_then(|h| h.shorthand().map(str::to_string))
                })
        };
        entries.push(json!({
            "path": path_str,
            "branch": branch,
            "last_agent_tool_ts": latest_mtime.map(iso8601),
            "agent_ids": agent_ids,
            "stale": stale,
        }));
    }

    let _ = repo_root; // keep signature symmetric with worktree_state for future use
    Ok(json!({
        "stale_minutes": stale_minutes,
        "worktrees": entries,
    }))
}

/// Walk `/tmp/agent-*/*/tasks/*.output` + `/tmp/agent-*.progress`
/// and assemble per-task observations. Crate-internal: kept for the
/// agent_inflight_summary helper plus the in-module test harness.
#[allow(dead_code)]
pub(crate) fn scan_observations_value() -> Result<Value, String> {
    let obs = scan_observations()?;
    let arr: Vec<Value> = obs
        .iter()
        .map(|o| {
            json!({
                "task_id": o.task_id,
                "last_mtime": iso8601(o.last_mtime),
                "last_tool": o.last_tool.clone(),
                "associated_worktree": o.associated_worktree.as_ref().map(|p| p.display().to_string()),
            })
        })
        .collect();
    Ok(json!({ "observations": arr }))
}

fn scan_observations() -> Result<Vec<AgentObservation>, String> {
    use std::collections::HashMap;
    let mut by_id: HashMap<String, AgentObservation> = HashMap::new();

    // Legacy `*.output` scan.
    for tasks_dir in glob_task_dirs() {
        let entries = match fs::read_dir(&tasks_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("output") {
                continue;
            }
            let task_id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if task_id.is_empty() {
                continue;
            }
            // Symlinks point at the real transcript; resolve so mtime
            // reflects the parent's writer activity not the symlink
            // creation moment.
            let target = p.canonicalize().unwrap_or(p.clone());
            let mtime = fs::metadata(&target)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            let assoc = scan_jsonl_for_worktree(&target);
            by_id
                .entry(task_id.clone())
                .and_modify(|e| {
                    if mtime > e.last_mtime {
                        e.last_mtime = mtime;
                    }
                    if e.associated_worktree.is_none() {
                        e.associated_worktree = assoc.clone();
                    }
                })
                .or_insert(AgentObservation {
                    task_id,
                    last_mtime: mtime,
                    associated_worktree: assoc,
                    last_tool: None,
                });
        }
    }

    // Heartbeat sentinel scan.
    if let Ok(read) = fs::read_dir("/tmp") {
        for entry in read.flatten() {
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if !name_str.starts_with("agent-") || !name_str.ends_with(".progress") {
                continue;
            }
            let task_id = name_str
                .trim_start_matches("agent-")
                .trim_end_matches(".progress")
                .to_string();
            let p = entry.path();
            let mtime = fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            let line = fs::read_to_string(&p).unwrap_or_default();
            let line = line.lines().next().unwrap_or("").to_string();
            // Format: "<ISO-ts> <tool> <truncated-args>"
            let mut parts = line.splitn(3, ' ');
            let _ts = parts.next();
            let tool = parts.next().map(str::to_string);
            let args = parts.next().unwrap_or("");
            let assoc_args = extract_worktree_path(args);
            by_id
                .entry(task_id.clone())
                .and_modify(|e| {
                    if mtime > e.last_mtime {
                        e.last_mtime = mtime;
                    }
                    if e.last_tool.is_none() {
                        e.last_tool = tool.clone();
                    }
                    if e.associated_worktree.is_none() {
                        e.associated_worktree = assoc_args.clone();
                    }
                })
                .or_insert(AgentObservation {
                    task_id,
                    last_mtime: mtime,
                    associated_worktree: assoc_args,
                    last_tool: tool,
                });
        }
    }

    let mut out: Vec<AgentObservation> = by_id.into_values().collect();
    out.sort_by(|a, b| b.last_mtime.cmp(&a.last_mtime));
    Ok(out)
}

fn glob_task_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Walks /tmp/<prefix><uid>/-workspace/<session-id>/tasks for each
    // session-prefixed directory under /tmp. Default prefix is `claude-`
    // (compatible with the Claude Code CLI transcript layout); override
    // via WTPOOL_TASK_DIR_PREFIX.
    let prefix = std::env::var("WTPOOL_TASK_DIR_PREFIX")
        .unwrap_or_else(|_| "claude-".to_string());
    if let Ok(top) = fs::read_dir("/tmp") {
        for top_entry in top.flatten() {
            let name = top_entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if !name_str.starts_with(&prefix) {
                continue;
            }
            let session_root = top_entry.path();
            let workspace = session_root.join("-workspace");
            let sessions = match fs::read_dir(&workspace) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for sess in sessions.flatten() {
                let tasks = sess.path().join("tasks");
                if tasks.is_dir() {
                    out.push(tasks);
                }
            }
        }
    }
    out
}

/// Look at the first ~20 KiB of the JSONL transcript for a substring
/// that looks like a worktree path. Best-effort, fail-quiet.
fn scan_jsonl_for_worktree(path: &Path) -> Option<PathBuf> {
    let buf = fs::read_to_string(path).ok()?;
    // Slice the first ~20 KiB but step backwards to a UTF-8 char
    // boundary so we never panic on a multibyte boundary mid-rune.
    // JSONL transcripts contain freely-pasted prompt text including
    // box-drawing chars, em-dashes, etc.
    let mut cap = buf.len().min(20_000);
    while cap > 0 && !buf.is_char_boundary(cap) {
        cap -= 1;
    }
    let head = &buf[..cap];
    extract_worktree_path(head)
}

/// Find first `/tmp/wtpool/<wt-name>` substring in
/// the input and return up through the `<wt-name>` segment.
pub(crate) fn extract_worktree_path(s: &str) -> Option<PathBuf> {
    let needle = "/tmp/wtpool/";
    let idx = s.find(needle)?;
    let tail = &s[idx..];
    // Take up through `/tmp/wtpool/<name>` and stop
    // at next `/`, whitespace, quote, or close-brace.
    let stop_at = tail
        .char_indices()
        .skip(needle.len())
        .find(|(_, c)| matches!(c, '/' | ' ' | '"' | ',' | '}' | '\n' | '\t' | '\r'))
        .map(|(i, _)| i)
        .unwrap_or(tail.len());
    Some(PathBuf::from(&tail[..stop_at]))
}

/// Last-ditch worktree association: when the heartbeat-args + JSONL
/// substring scans both came up empty, attempt to match the
/// observation's task-id against the names of known worktrees. The
/// parent dispatch occasionally uses the worktree name itself (or a
/// descriptive prefix of it) as the subagent task-id, so a substring
/// match catches the common case without false-positiving on
/// unrelated nanoid IDs.
///
/// Longer worktree names are checked first so an exact-prefix match
/// wins over a coincidental short-substring collision (e.g.
/// `audio-runtime-phase2` beats `audio` for an `audio-runtime-…` ID).
/// Returns the first matching worktree path or `None` when no name is
/// a substring of the task-id.
fn associate_via_known(obs: &AgentObservation, known: &[PathBuf]) -> Option<PathBuf> {
    if obs.task_id.is_empty() {
        return None;
    }
    let mut candidates: Vec<&PathBuf> = known
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|name| !name.is_empty() && obs.task_id.contains(name))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort_by(|a, b| {
        let an = a.file_name().and_then(|n| n.to_str()).unwrap_or("").len();
        let bn = b.file_name().and_then(|n| n.to_str()).unwrap_or("").len();
        bn.cmp(&an)
    });
    candidates.first().map(|p| (*p).clone())
}

/// RFC 3339 / ISO-8601 formatter for telemetry timestamps. Re-exported
/// for `reviews` to avoid pulling chrono into the dep graph just for a
/// single timestamp; both modules want identical output.
pub(crate) fn iso8601_for_test(t: SystemTime) -> String {
    iso8601(t)
}

fn iso8601(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Avoid pulling chrono as a dependency for a single timestamp;
    // emit RFC 3339 by hand. Local TZ ignored — UTC is the canonical
    // form for telemetry timestamps.
    let secs_i = secs as i64;
    let days_since_epoch = secs_i / 86_400;
    let secs_in_day = secs_i.rem_euclid(86_400);
    let (y, m, d) = days_to_ymd(days_since_epoch);
    let hh = secs_in_day / 3600;
    let mm = (secs_in_day / 60) % 60;
    let ss = secs_in_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days-since-1970-01-01 → (year, month, day). Plain Gregorian, no
/// dependency. Reference: civil_from_days, Howard Hinnant 2010.
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn extract_worktree_path_finds_canonical_form() {
        let s = "args: cd /tmp/wtpool/foo-impl && git status";
        let p = extract_worktree_path(s).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/wtpool/foo-impl"));
    }

    #[test]
    fn extract_worktree_path_handles_quoted_path() {
        let s = r#"{"cwd": "/tmp/wtpool/bar"}"#;
        let p = extract_worktree_path(s).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/wtpool/bar"));
    }

    #[test]
    fn extract_worktree_path_returns_none_when_absent() {
        assert!(extract_worktree_path("nothing here").is_none());
    }

    #[test]
    fn iso8601_round_trip_known_epoch() {
        let t = UNIX_EPOCH + Duration::from_secs(0);
        assert_eq!(iso8601(t), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // 1700000000 = 2023-11-14T22:13:20Z
        assert_eq!(iso8601(t), "2023-11-14T22:13:20Z");
    }

    fn obs_with_id(id: &str) -> AgentObservation {
        AgentObservation {
            task_id: id.to_string(),
            last_mtime: UNIX_EPOCH,
            associated_worktree: None,
            last_tool: None,
        }
    }

    #[test]
    fn associate_via_known_matches_worktree_name_in_task_id() {
        // Common dispatch convention: parent encodes worktree name in
        // task id (or as descriptive prefix). Fallback path picks it
        // up when args + JSONL substring scans miss.
        let known = vec![
            PathBuf::from("/tmp/wtpool/audio-runtime-phase2"),
            PathBuf::from("/tmp/wtpool/foo"),
        ];
        let obs = obs_with_id("audio-runtime-phase2-step1-7f3a");
        let p = associate_via_known(&obs, &known).expect("match");
        assert_eq!(
            p,
            PathBuf::from("/tmp/wtpool/audio-runtime-phase2")
        );
    }

    #[test]
    fn associate_via_known_prefers_longer_name_on_substring_collision() {
        // `audio` is a substring of `audio-runtime-phase2` — the
        // longer (more specific) match must win so coincidental short
        // names don't shadow exact prefixes.
        let known = vec![
            PathBuf::from("/tmp/wtpool/audio"),
            PathBuf::from("/tmp/wtpool/audio-runtime-phase2"),
        ];
        let obs = obs_with_id("audio-runtime-phase2-impl-001");
        let p = associate_via_known(&obs, &known).expect("match");
        assert_eq!(
            p,
            PathBuf::from("/tmp/wtpool/audio-runtime-phase2")
        );
    }

    #[test]
    fn associate_via_known_returns_none_when_no_substring() {
        let known = vec![PathBuf::from("/tmp/wtpool/foo-impl")];
        let obs = obs_with_id("unrelated-nanoid-xyz123");
        assert!(associate_via_known(&obs, &known).is_none());
    }

    #[test]
    fn associate_via_known_handles_empty_task_id() {
        let known = vec![PathBuf::from("/tmp/wtpool/foo")];
        let obs = obs_with_id("");
        assert!(associate_via_known(&obs, &known).is_none());
    }

    #[test]
    fn agent_inflight_summary_handles_empty_tmp() {
        // We cannot guarantee /tmp is empty, but the call must not
        // panic and must return a `worktrees` array.
        let v = agent_inflight_summary(Path::new("/repo"), 5, &[]).unwrap();
        assert!(v["worktrees"].is_array());
    }
}
