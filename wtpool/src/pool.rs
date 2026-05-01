//! Pool-worktree acquire / release / status RPCs.
//!
//! Mechanistic enforcement of the M5 pool-first directive (2026-04-27):
//! parent-side dispatchers should not have to manually run
//! `worktree_list`, filter detached+clean entries, pick a slot path,
//! and inline a `git checkout -b` step into every dispatch prompt.
//! Three RPCs replace that dance:
//!
//! - [`pool_acquire`] — claim the first detached+clean slot under
//!   `<repo>/wt-pool/wt-*`, create the requested branch
//!   on it via `git checkout -b <branch> <base_sha>`, return the
//!   absolute path. Race-safe via a per-slot atomic lockfile created
//!   with `O_EXCL` semantics (`OpenOptions::create_new`).
//! - [`pool_release`] — reverse: detach the slot, hard-reset to main,
//!   `git clean -fdx -e .cargo` (drops untracked AND ignored files,
//!   preserves the worktree-template `.cargo/config.toml`), then
//!   verifies the slot is detached at main tip + clean. Refuses to
//!   release if the branch has commits ahead of main that have NOT
//!   been merged into main, OR if the working tree is dirty, unless
//!   the caller passes `force=true`.
//! - [`pool_status`] — census of free slots vs in-use slots.
//!
//! All paths returned are absolute. Pool root is fixed at
//! `<repo>/wt-pool`. Slot directories follow the
//! `wt-NN` convention (zero-padded two-digit suffix), pre-created by
//! `tools/worktree-pool-init.sh` or the manual `git worktree add`
//! cascade documented in `docs/research/2026-04-27-worktree-pool*`.
//!
//! "Detached" here means `HEAD` is not a branch ref — `git checkout
//! --detach <sha>` state. "Clean" means `git status --porcelain`
//! produces no output (no tracked-modified, no untracked).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use git2::{Repository, StatusOptions};
use serde_json::{json, Value};

use crate::git::open_repo;
use crate::git_exec::git;

/// Compute the per-slot lockfile path. Lives in `/tmp/` (NOT inside
/// the slot working directory) so the lockfile itself does not show
/// up as an untracked file and confuse the dirty-flag classifier.
/// Filename embeds a hash of the canonicalised slot path so distinct
/// slots map to distinct lockfiles even when they share a basename
/// across temp dirs (test fixtures).
///
/// The lockfile is a plain marker file held under an `flock(2)`
/// exclusive lock for the duration of the acquire window. The file
/// itself is allowed to persist across calls — the kernel-managed
/// advisory lock, not the file's existence, is what serialises
/// concurrent acquires. This is the LOW-PR2 fix (2026-04-28) for the
/// crash-resilience hole the prior `O_EXCL` marker scheme had: a
/// process that crashed mid-acquire left the marker on disk, and
/// every subsequent `create_new` failed → "all slots lost lockfile
/// race" → "pool exhausted (...)" until manual `/tmp/wtpool-*`
/// cleanup. With flock, the kernel releases the lock when the
/// holding process's FD is closed (including on crash / SIGKILL /
/// segfault), so the next acquire can take the lock without
/// observing any stale state.
fn lock_path_for(slot: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let canon = slot.canonicalize().unwrap_or_else(|_| slot.to_path_buf());
    let mut h = DefaultHasher::new();
    canon.hash(&mut h);
    PathBuf::from(format!("/tmp/wtpool-acquire-{:016x}.lock", h.finish()))
}

/// Pool root relative to repo: `<repo>/wt-pool/`.
fn pool_root(repo_root: &Path) -> PathBuf {
    repo_root.join(".claude").join("worktrees").join("pool")
}

/// Census of pool state at a given moment. Returned by
/// [`pool_status`] and used internally by [`pool_acquire`] /
/// [`pool_release`] for free-slot discovery + sanity checks.
struct SlotInventory {
    /// Slots whose HEAD is detached AND working tree is clean. Eligible
    /// for `pool_acquire`. Sorted by path so acquire order is
    /// deterministic across calls.
    free: Vec<PathBuf>,
    /// Slots that hold a branch (`HEAD -> refs/heads/<branch>`) or are
    /// dirty. `Vec<(path, branch_or_detached, commits_ahead)>`.
    in_use: Vec<(PathBuf, String, u64)>,
}

/// Walk the pool root, classify each `wt-*` slot as free or in-use.
/// Errors on any individual slot are swallowed (slot reported as
/// in-use with branch=`<error>`); the overall function only fails when
/// the pool root itself is missing.
fn inventory(repo_root: &Path) -> Result<SlotInventory, String> {
    let pool = pool_root(repo_root);
    if !pool.is_dir() {
        return Err(format!(
            "pool not found at {} — create with `tools/worktree-pool-init.sh`",
            pool.display()
        ));
    }
    let main_repo = open_repo(repo_root)?;
    let main_tip = main_repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map(|c| c.id())
        .map_err(|e| format!("git2: main HEAD oid: {e}"))?;

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&pool)
        .map_err(|e| format!("read pool root {}: {e}", pool.display()))?
        .filter_map(|r| r.ok())
        .map(|d| d.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("wt-"))
        })
        .collect();
    entries.sort();

    let mut free = Vec::new();
    let mut in_use = Vec::new();
    for slot in entries {
        match classify_slot(&slot, main_tip) {
            Ok(SlotState::Free) => free.push(slot),
            Ok(SlotState::InUse { branch, ahead }) => in_use.push((slot, branch, ahead)),
            Err(e) => in_use.push((slot, format!("<error: {e}>"), 0)),
        }
    }
    Ok(SlotInventory { free, in_use })
}

enum SlotState {
    Free,
    InUse { branch: String, ahead: u64 },
}

/// Classify a single slot. Detached + clean → Free. Anything else
/// (branch-checkout, dirty working tree, dirty index) → InUse.
fn classify_slot(slot: &Path, main_tip: git2::Oid) -> Result<SlotState, String> {
    let repo = Repository::open(slot).map_err(|e| format!("open {}: {e}", slot.display()))?;
    let head = repo.head().map_err(|e| format!("head: {e}"))?;
    let dirty = is_dirty(&repo)?;
    let detached = !head.is_branch();
    if detached && !dirty {
        return Ok(SlotState::Free);
    }
    let branch = if detached {
        "<detached>".to_string()
    } else {
        head.shorthand().unwrap_or("<unknown>").to_string()
    };
    let head_oid = head
        .peel_to_commit()
        .map(|c| c.id())
        .map_err(|e| format!("head peel: {e}"))?;
    let ahead = if head_oid == main_tip {
        0
    } else {
        repo.graph_ahead_behind(head_oid, main_tip)
            .map(|(a, _)| a as u64)
            .unwrap_or(0)
    };
    Ok(SlotState::InUse { branch, ahead })
}

/// True iff the worktree has any tracked-modified, staged, or
/// untracked files OTHER than the worktree-template `.cargo/`
/// directory. The `.cargo/` exemption matches the `pool_release`
/// clean-step exemption (`git clean -fdx -e .cargo`): a pool slot
/// that holds only a per-worktree `.cargo/config.toml` (untracked
/// by design — see `tools/worktree-template/.cargo/config.toml`)
/// must classify as Free in `classify_slot` AND must satisfy the
/// `pool_release` post-condition. Without this exemption, releasing
/// a slot that had `.cargo/` would loop: classify_slot sees
/// untracked `.cargo/`, marks slot InUse, pool_acquire skips it,
/// AND post-release verification trips on the same untracked entry.
/// Ignored files (e.g. `target/`) are NOT included in StatusOptions
/// (default), so this function does not see them — `pool_release`
/// reaches them via `git clean -fdx`.
fn is_dirty(repo: &Repository) -> Result<bool, String> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let s = repo
        .statuses(Some(&mut opts))
        .map_err(|e| format!("statuses: {e}"))?;
    for entry in s.iter() {
        let path = match entry.path() {
            Some(p) => p,
            None => return Ok(true),
        };
        if path == ".cargo" || path.starts_with(".cargo/") {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// `pool_status` body. Returns `{free: [{path, head_sha}], in_use:
/// [{path, branch, commits_ahead}]}`.
pub fn pool_status(repo_root: &Path) -> Result<Value, String> {
    let inv = inventory(repo_root)?;
    let free: Vec<Value> = inv
        .free
        .iter()
        .map(|p| {
            let head_sha = Repository::open(p)
                .ok()
                .and_then(|r| {
                    r.head()
                        .ok()
                        .and_then(|h| h.peel_to_commit().ok())
                        .map(|c| c.id().to_string())
                })
                .unwrap_or_else(|| "<unresolved>".to_string());
            json!({ "path": p, "head_sha": head_sha })
        })
        .collect();
    let in_use: Vec<Value> = inv
        .in_use
        .iter()
        .map(|(p, b, a)| json!({ "path": p, "branch": b, "commits_ahead": a }))
        .collect();
    Ok(json!({ "free": free, "in_use": in_use }))
}

/// `pool_acquire` body. Find the first detached+clean slot, atomically
/// lock it, optionally run `git checkout -b <branch_name> <base_sha>`
/// on the slot, return the slot path + resolved base sha.
///
/// **Atomic-acquire semantics (P0 2026-04-28).** The slot's branch is
/// always created at the *current* `main` tip when `base_sha` is
/// `None` — never at the slot's stale detached HEAD (which can lag
/// main by hours/days/weeks if the slot has been idle). Pre-existing
/// detached HEAD ≠ implicit base; the explicit start point on
/// `git checkout -b` repoints the working tree forward to the
/// resolved base. This is what makes pool-acquire equivalent to
/// "fresh worktree at current main" without paying the per-create
/// 10-30s checkout cost.
///
/// `branch_name` is **optional**. When `Some`, behaves as above:
/// branch is created on the slot, returned in the response. When
/// `None`, the slot is returned still detached at the resolved base
/// sha — the caller is responsible for any branch creation. The
/// detached path exists for low-level callers who want non-standard
/// branch operations (e.g. checkout an existing branch, multi-step
/// git dance) on a slot they own.
///
/// `base_sha` defaults to the current `main` tip when `None`. The
/// resolved sha is included in the response so callers can confirm
/// what they actually got.
///
/// Concurrency: two simultaneous acquires racing for the same free
/// slot are serialised by an `O_EXCL` lockfile create at
/// `/tmp/wtpool-acquire-<hash>.lock`. The losing call falls through
/// to the next free slot. When all slots lose the race the call
/// returns the same `pool exhausted` error a steady-state-exhausted
/// call would.
pub fn pool_acquire(
    repo_root: &Path,
    branch_name: Option<&str>,
    base_sha: Option<&str>,
) -> Result<Value, String> {
    if let Some(name) = branch_name {
        validate_branch_name(name)?;
    }
    // Resolve base sha — explicit arg if provided, else main tip.
    let main_repo = open_repo(repo_root)?;
    let resolved_base = match base_sha {
        Some(s) => {
            // Verify the sha exists in the repo so we surface a clear
            // error here rather than from `git checkout`.
            let oid = git2::Oid::from_str(s)
                .map_err(|e| format!("base_sha {s:?} not a valid hex oid: {e}"))?;
            main_repo
                .find_commit(oid)
                .map_err(|e| format!("base_sha {s} not in repo: {e}"))?;
            s.to_string()
        }
        None => main_repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map(|c| c.id().to_string())
            .map_err(|e| format!("resolve main tip: {e}"))?,
    };

    // Refuse if a worktree (anywhere) already holds this branch.
    if let Some(name) = branch_name {
        if branch_in_use(&main_repo, name) {
            return Err(format!(
                "branch {name:?} already checked out in another worktree"
            ));
        }
    }

    let inv = inventory(repo_root)?;
    if inv.free.is_empty() {
        let in_use_branches: Vec<String> = inv
            .in_use
            .iter()
            .map(|(p, b, _)| format!("{}={}", p.display(), b))
            .collect();
        return Err(format!(
            "pool exhausted ({} alive): {}",
            inv.in_use.len(),
            in_use_branches.join(", ")
        ));
    }

    // Try slots in order, holding an exclusive `flock(2)` on a per-
    // slot marker file for each attempt. The first slot we
    // successfully lock + verify-still-free wins. The lock is
    // released automatically when `lock` goes out of scope (RAII via
    // `File`'s Drop closing the FD), AND the kernel releases the
    // lock if the holding process dies before reaching that drop —
    // the crash-resilience guarantee LOW-PR2 calls for. The marker
    // file is left in place after release; future acquires re-open
    // it (idempotent) and re-flock. There is no `remove_file` step.
    for slot in &inv.free {
        let lock_path = lock_path_for(slot);
        let lock = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(_) => continue, // can't open lockfile path; skip slot
        };
        // Try non-blocking exclusive lock. If another process holds
        // it the call returns `WouldBlock` (or platform-equivalent)
        // and we move on to the next free slot. If the previous
        // holder crashed without unlocking, the kernel released the
        // lock on FD close, so this call succeeds.
        if FileExt::try_lock_exclusive(&lock).is_err() {
            continue; // contended; try next slot
        }
        // Re-verify free now we hold the lock — earlier classification
        // can race with another acquire that completed between
        // inventory() and here.
        let main_tip = main_repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map(|c| c.id())
            .map_err(|e| format!("re-resolve main tip: {e}"))?;
        match classify_slot(slot, main_tip) {
            Ok(SlotState::Free) => {}
            Ok(_) | Err(_) => {
                // Drop releases the flock; lockfile persists (harmless).
                drop(lock);
                continue;
            }
        }
        // Atomic-acquire: drive the slot to `resolved_base` regardless
        // of where its detached HEAD currently sits. When the caller
        // supplied `branch_name`, create the branch at that base via
        // `git checkout -b`. Otherwise advance the slot's detached
        // HEAD to the base via `git checkout --detach <base>` so the
        // returned slot's working tree matches `resolved_base`. Either
        // path makes the post-condition "slot HEAD == resolved_base".
        // This is the load-bearing step that prevents the stale-base
        // BOUNCE_BACK class — a slot detached at last-week's main
        // tip becomes a fresh-from-current-main checkout in <1s.
        if let Some(name) = branch_name {
            let out = git(slot, &["checkout", "-b", name, &resolved_base])?;
            if !out.success() {
                drop(lock);
                return Err(format!(
                    "git checkout -b {name} {resolved_base} on {} failed: {}",
                    slot.display(),
                    out.stderr.trim()
                ));
            }
        } else {
            let out = git(slot, &["checkout", "--detach", &resolved_base])?;
            if !out.success() {
                drop(lock);
                return Err(format!(
                    "git checkout --detach {resolved_base} on {} failed: {}",
                    slot.display(),
                    out.stderr.trim()
                ));
            }
        }
        drop(lock);
        let mut payload = json!({
            "path": slot,
            "base_sha": resolved_base,
        });
        if let Some(name) = branch_name {
            payload
                .as_object_mut()
                .unwrap()
                .insert("branch".into(), Value::String(name.to_string()));
        }
        return Ok(payload);
    }
    Err(format!(
        "pool exhausted (all {} free slots lost lockfile race)",
        inv.free.len()
    ))
}

/// Returns true if `branch_name` is checked out in any worktree of
/// the repo (main checkout or any linked worktree). Used by
/// `pool_acquire` to fail fast before we even pick a slot.
fn branch_in_use(repo: &Repository, branch_name: &str) -> bool {
    let target_ref = format!("refs/heads/{branch_name}");
    // Main checkout
    if let Ok(head) = repo.head() {
        if head.name() == Some(target_ref.as_str()) {
            return true;
        }
    }
    let names = match repo.worktrees() {
        Ok(n) => n,
        Err(_) => return false,
    };
    for i in 0..names.len() {
        let name = match names.get(i) {
            Some(n) => n,
            None => continue,
        };
        let wt = match repo.find_worktree(name) {
            Ok(w) => w,
            Err(_) => continue,
        };
        if let Ok(wt_repo) = Repository::open(wt.path()) {
            if let Ok(head) = wt_repo.head() {
                if head.name() == Some(target_ref.as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

/// `pool_release` body. Detach the slot, hard-reset to main, clean
/// untracked AND ignored files (preserving `.cargo/`). Refuses to
/// release when the slot's branch has commits ahead of main that have
/// not been merged into main (would silently lose work) — caller must
/// pass `force=true` to override.
///
/// `path` must be an absolute path inside `<repo>/wt-pool/
/// pool/`. Slots outside the pool root are rejected so a wayward call
/// can't accidentally nuke `wt-pool/<feature-branch>/`.
///
/// **Cleaning policy (P0 2026-04-28).** The clean step runs as
/// `git clean -fdx -e .cargo`:
/// - `-x` removes ignored files too (e.g. `target/`, sibling build
///   artifacts). Without `-x`, accumulated build cruft persists across
///   release cycles; classify_slot's `is_dirty()` doesn't see ignored
///   files (default `StatusOptions` excludes them), so a slot with
///   only ignored leftovers stays "free" + the next `pool_acquire`
///   hands it out with prior-session state in place.
/// - `-e .cargo` exempts the worktree-template-copied
///   `.cargo/config.toml` (per-worktree `target-dir = "./target"`
///   override per `tools/worktree-template/`). That file is
///   *untracked* — `-fd` would wipe it without the exempt. Without it,
///   subsequent builds in the slot share `/tmp/wtpool-target/` with
///   sibling worktrees, reintroducing the cross-crate
///   feature-unification breakage class CLAUDE.md documents.
///
/// **Post-condition.** After successful release the slot is
/// `(detached HEAD)` at `main_tip` AND `git status --porcelain` is
/// empty. Verified before returning so callers can trust the slot is
/// pristine. Verification failure surfaces as a release error rather
/// than a silent half-clean.
pub fn pool_release(repo_root: &Path, path: &Path, force: bool) -> Result<Value, String> {
    if !path.is_absolute() {
        return Err(format!("path must be absolute, got {}", path.display()));
    }
    let pool = pool_root(repo_root);
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let pool_canon = pool.canonicalize().unwrap_or_else(|_| pool.clone());
    if !canon.starts_with(&pool_canon) {
        return Err(format!(
            "{} is outside pool root {}",
            canon.display(),
            pool_canon.display()
        ));
    }

    // Sanity: must be a worktree directory of `repo_root`.
    let slot_repo =
        Repository::open(&canon).map_err(|e| format!("open slot {}: {e}", canon.display()))?;
    let main_repo = open_repo(repo_root)?;
    let main_tip = main_repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map(|c| c.id())
        .map_err(|e| format!("resolve main tip: {e}"))?;

    // Refuse-release-with-dirty-tree gate. Independent of the
    // branch/detached distinction below: if the working tree has
    // uncommitted changes (tracked-modified, staged, or untracked),
    // the subsequent `reset --hard` + `clean -fd` would silently
    // discard them. Detached HEAD with edits is the recovery-path
    // footgun MED-PR1 (torvalds + lattner co-flagged 2026-04-27):
    // the branch-only gate at `head.is_branch()` below misses this
    // case entirely. Force overrides for the `pool_release(force=
    // true)` callers that genuinely want a hard reset (e.g. cleanup
    // after a stuck slot).
    if !force {
        let dirty = is_dirty(&slot_repo)?;
        if dirty {
            return Err(format!(
                "refuse to release {}: working tree dirty (uncommitted changes that reset --hard + clean -fd would discard). Pass force=true to override.",
                canon.display()
            ));
        }
    }

    // Refuse-release-with-unmerged-commits gate. A branch is
    // "unmerged" when its tip has commits ahead of main AND the tip
    // is NOT itself reachable from main (i.e. a fast-forward merge of
    // the branch would change main's tip).
    let head = slot_repo.head().map_err(|e| format!("slot head: {e}"))?;
    if head.is_branch() && !force {
        let head_oid = head
            .peel_to_commit()
            .map(|c| c.id())
            .map_err(|e| format!("slot head peel: {e}"))?;
        if head_oid != main_tip {
            let (ahead, _) = slot_repo
                .graph_ahead_behind(head_oid, main_tip)
                .map_err(|e| format!("graph_ahead_behind: {e}"))?;
            // If ahead > 0 AND head is not reachable from main, the
            // branch holds work that would be lost on release.
            if ahead > 0 && !is_reachable_from(&main_repo, head_oid, main_tip) {
                let branch_name = head.shorthand().unwrap_or("<unknown>");
                return Err(format!(
                    "refuse to release {}: branch {} has {} commit(s) ahead of main not merged in. Pass force=true to override.",
                    canon.display(),
                    branch_name,
                    ahead
                ));
            }
        }
    }

    // Detach + reset + clean. Done via `git_exec` so we get stderr
    // capture if any of the three steps fail.
    let detach = git(&canon, &["checkout", "--detach", &main_tip.to_string()])?;
    if !detach.success() {
        return Err(format!(
            "git checkout --detach failed: {}",
            detach.stderr.trim()
        ));
    }
    let reset = git(&canon, &["reset", "--hard", &main_tip.to_string()])?;
    if !reset.success() {
        return Err(format!("git reset --hard failed: {}", reset.stderr.trim()));
    }
    // `-fdx` so ignored files (target/, build cruft) are also wiped —
    // is_dirty() doesn't see ignored files, so leaving them behind
    // means the slot looks "free" to classify_slot but hands out
    // prior-session state to the next pool_acquire. `-e .cargo`
    // exempts the worktree-template-copied .cargo/config.toml so the
    // per-worktree target-dir override survives release.
    let clean = git(&canon, &["clean", "-fdx", "-e", ".cargo"])?;
    if !clean.success() {
        return Err(format!(
            "git clean -fdx -e .cargo failed: {}",
            clean.stderr.trim()
        ));
    }

    // Best-effort: delete the now-orphan branch from the main repo's
    // ref store so subsequent `pool_acquire` calls can re-use the
    // branch name. Failure is non-fatal — caller might want to keep
    // the branch around for a follow-up.
    if head.is_branch() {
        if let Some(branch_name) = head.shorthand() {
            let _ = git(repo_root, &["branch", "-D", branch_name]);
        }
    }

    // Post-condition verification (P0 2026-04-28): re-open the slot,
    // confirm HEAD is detached at main_tip AND working tree is clean.
    // Surfaces silent half-cleans (e.g. detach succeeded but a clean
    // step didn't reach all of the cruft) as a release error rather
    // than a returned `released: true` over a still-dirty slot.
    verify_post_release(&canon, main_tip)?;

    Ok(json!({ "released": true, "path": canon }))
}

/// Post-condition verification for `pool_release`: re-opens the slot,
/// asserts HEAD is detached at `main_tip` AND working tree is clean.
/// Extracted so unit tests can construct torn states (HEAD-on-branch,
/// HEAD-at-wrong-SHA, dirty tracked file) and probe the verify path
/// independently of the detach + reset + clean steps that precede it
/// in `pool_release`.
fn verify_post_release(canon: &Path, main_tip: git2::Oid) -> Result<(), String> {
    let post_repo = Repository::open(canon)
        .map_err(|e| format!("post-release open {}: {e}", canon.display()))?;
    let post_head = post_repo
        .head()
        .map_err(|e| format!("post-release head: {e}"))?;
    if post_head.is_branch() {
        return Err(format!(
            "post-release verification failed: {} HEAD is still on a branch ({})",
            canon.display(),
            post_head.shorthand().unwrap_or("<unknown>")
        ));
    }
    let post_tip = post_head
        .peel_to_commit()
        .map(|c| c.id())
        .map_err(|e| format!("post-release peel: {e}"))?;
    if post_tip != main_tip {
        return Err(format!(
            "post-release verification failed: {} HEAD {} != main_tip {}",
            canon.display(),
            post_tip,
            main_tip
        ));
    }
    if is_dirty(&post_repo)? {
        return Err(format!(
            "post-release verification failed: {} working tree still dirty after clean",
            canon.display()
        ));
    }
    Ok(())
}

/// Is `commit` reachable from `from`? Used by `pool_release` to
/// distinguish "branch tip already merged into main" (safe to
/// release) from "branch tip has unmerged work" (refuse without
/// force).
fn is_reachable_from(repo: &Repository, commit: git2::Oid, from: git2::Oid) -> bool {
    let mut walker = match repo.revwalk() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if walker.push(from).is_err() {
        return false;
    }
    walker.flatten().any(|oid| oid == commit)
}

/// Light validation: branch names must be non-empty and free of
/// characters that would break shell-quoting or git ref-name rules.
/// Defers heavy validation to git itself (will reject e.g. trailing
/// `.lock`, double slashes, …) but rejects the obvious shell-injection
/// shapes here so the error message stays readable.
fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("branch_name cannot be empty".into());
    }
    if name.starts_with('-') {
        return Err(format!("branch_name {name:?} cannot start with `-`"));
    }
    if name.contains("..") {
        return Err(format!("branch_name {name:?} cannot contain `..`"));
    }
    let bad = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.')));
    if let Some(c) = bad {
        return Err(format!(
            "branch_name {name:?} contains disallowed character {c:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;
    use tempfile::TempDir;

    /// Build a temp repo with N pool slots, all initially detached at
    /// main's tip. Returns `(tmp, repo_root)`. The repo has one main
    /// commit on `main`.
    fn fixture_with_pool(n_slots: usize) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let repo = Repository::init(&repo_root).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap();
        // One main commit so HEAD is resolvable.
        commit_file(&repo, &sig, "seed.txt", "seed", "main: seed");
        // Rename HEAD to main if needed.
        if let Ok(head) = repo.head() {
            if head.is_branch() && head.shorthand() == Some("master") {
                let c = repo.head().unwrap().peel_to_commit().unwrap();
                repo.branch("main", &c, true).unwrap();
                repo.set_head("refs/heads/main").unwrap();
            }
        }
        let main_commit = repo.head().unwrap().peel_to_commit().unwrap();
        // Pool root + N slots, each a detached worktree at main's tip.
        let pool = repo_root.join(".claude").join("worktrees").join("pool");
        fs::create_dir_all(&pool).unwrap();
        for i in 1..=n_slots {
            let slot_name = format!("wt-{i:02}");
            let slot_path = pool.join(&slot_name);
            // Create the worktree on a temporary holder branch so
            // libgit2 doesn't complain; immediately detach inside the
            // slot.
            let holder_branch_name = format!("__pool_holder_{slot_name}");
            let holder = repo
                .branch(&holder_branch_name, &main_commit, true)
                .unwrap();
            let mut opts = git2::WorktreeAddOptions::new();
            opts.reference(Some(holder.get()));
            repo.worktree(&slot_name, &slot_path, Some(&opts)).unwrap();
            // Detach inside the slot so it counts as Free.
            let slot_repo = Repository::open(&slot_path).unwrap();
            slot_repo.set_head_detached(main_commit.id()).unwrap();
            // Clean up the holder branch so it doesn't pollute the
            // ref store.
            let mut h = repo
                .find_branch(&holder_branch_name, git2::BranchType::Local)
                .unwrap();
            h.delete().unwrap();
        }
        (tmp, repo_root)
    }

    fn commit_file(repo: &Repository, sig: &Signature, name: &str, body: &str, msg: &str) {
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(name), body).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new(name)).unwrap();
        idx.write().unwrap();
        let tree_oid = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().map(|p| vec![p]).unwrap_or_default();
        repo.commit(Some("HEAD"), sig, sig, msg, &tree, &parents)
            .unwrap();
    }

    #[test]
    fn pool_status_lists_free_slots() {
        let (_tmp, root) = fixture_with_pool(3);
        let v = pool_status(&root).expect("status ok");
        assert_eq!(v["free"].as_array().unwrap().len(), 3);
        assert_eq!(v["in_use"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn pool_status_errors_when_pool_root_absent() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        Repository::init(&repo_root).unwrap();
        // No pool dir.
        let err = pool_status(&repo_root).expect_err("must error");
        assert!(err.contains("pool not found"), "got {err:?}");
    }

    #[test]
    fn acquire_then_release_round_trip() {
        let (_tmp, root) = fixture_with_pool(2);
        let res = pool_acquire(&root, Some("feat-roundtrip"), None).expect("acquire ok");
        let path = res["path"].as_str().unwrap();
        assert_eq!(res["branch"], "feat-roundtrip");
        // Acquire reduces free count.
        let s1 = pool_status(&root).unwrap();
        assert_eq!(s1["free"].as_array().unwrap().len(), 1);
        assert_eq!(s1["in_use"].as_array().unwrap().len(), 1);
        // Release returns it.
        let rel = pool_release(&root, Path::new(path), false).expect("release ok");
        assert_eq!(rel["released"], true);
        let s2 = pool_status(&root).unwrap();
        assert_eq!(s2["free"].as_array().unwrap().len(), 2);
        assert_eq!(s2["in_use"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn acquire_exhausted_reports_in_use_branches() {
        let (_tmp, root) = fixture_with_pool(1);
        pool_acquire(&root, Some("feat-only-slot"), None).unwrap();
        let err = pool_acquire(&root, Some("feat-second"), None).expect_err("must exhaust");
        assert!(err.contains("pool exhausted"), "got {err:?}");
        assert!(err.contains("feat-only-slot"), "got {err:?}");
    }

    #[test]
    fn acquire_rejects_duplicate_branch() {
        let (_tmp, root) = fixture_with_pool(2);
        pool_acquire(&root, Some("feat-dup"), None).unwrap();
        let err = pool_acquire(&root, Some("feat-dup"), None).expect_err("must reject duplicate");
        assert!(err.contains("already checked out"), "got {err:?}");
    }

    #[test]
    fn release_refuses_when_branch_has_unmerged_commits() {
        let (_tmp, root) = fixture_with_pool(1);
        let res = pool_acquire(&root, Some("feat-with-work"), None).unwrap();
        let path = res["path"].as_str().unwrap();
        // Add a commit on the branch.
        let slot_repo = Repository::open(path).unwrap();
        let sig = Signature::now("t", "t@e.com").unwrap();
        commit_file(&slot_repo, &sig, "work.txt", "work", "branch: work");
        // Release without force → refused.
        let err = pool_release(&root, Path::new(path), false).expect_err("must refuse");
        assert!(err.contains("ahead of main"), "got {err:?}");
        assert!(err.contains("force=true"), "got {err:?}");
        // Release with force → succeeds.
        let rel = pool_release(&root, Path::new(path), true).expect("force release ok");
        assert_eq!(rel["released"], true);
    }

    #[test]
    fn release_rejects_path_outside_pool() {
        let (_tmp, root) = fixture_with_pool(1);
        let err =
            pool_release(&root, Path::new("/tmp"), false).expect_err("outside-pool must reject");
        assert!(
            err.contains("outside pool root") || err.contains("open slot"),
            "got {err:?}"
        );
    }

    #[test]
    fn race_safety_two_concurrent_acquires_get_distinct_slots() {
        // Spawn two threads each calling pool_acquire concurrently.
        // Both must succeed (we have 2 slots) AND must land on
        // different paths (no double-claim).
        let (tmp, root) = fixture_with_pool(2);
        let _keep = tmp; // keep TempDir alive for thread duration
        let root_a = root.clone();
        let root_b = root.clone();
        let h1 = std::thread::spawn(move || pool_acquire(&root_a, Some("feat-race-a"), None));
        let h2 = std::thread::spawn(move || pool_acquire(&root_b, Some("feat-race-b"), None));
        let r1 = h1.join().unwrap().expect("thread1 acquire ok");
        let r2 = h2.join().unwrap().expect("thread2 acquire ok");
        let p1 = r1["path"].as_str().unwrap();
        let p2 = r2["path"].as_str().unwrap();
        assert_ne!(p1, p2, "two acquires landed on same slot: {p1} == {p2}");
    }

    #[test]
    fn race_safety_high_contention_8t_4slots_100iters() {
        // Stress version of the 2-thread race test. Per iteration:
        //   * fresh 4-slot fixture
        //   * 8 threads parked on a Barrier
        //   * release the barrier so all 8 slam pool_acquire at once
        //   * exactly 4 must succeed with distinct slot paths
        //   * exactly 4 must fail with `pool exhausted`
        //
        // The Barrier maximises contention at the O_EXCL window in
        // pool_acquire (lock_path_for(slot) → OpenOptions::create_new),
        // which is the load-bearing race the existing 2-thread test
        // only weakly probes.
        //
        // Expected wall time: 1-5s on the dev container (100 iters × 8
        // threads × per-iter libgit2 fixture init dominates over the
        // acquire cost itself).
        use std::sync::{Arc, Barrier};

        const ITERS: usize = 100;
        const THREADS: usize = 8;
        const SLOTS: usize = 4;

        for iter in 0..ITERS {
            let (tmp, root) = fixture_with_pool(SLOTS);
            let _keep = tmp; // hold TempDir for thread duration
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|tid| {
                    let root_c = root.clone();
                    let bar_c = barrier.clone();
                    let branch = format!("feat-stress-i{iter}-t{tid}");
                    std::thread::spawn(move || {
                        // Park here until all threads ready.
                        bar_c.wait();
                        pool_acquire(&root_c, Some(&branch), None)
                    })
                })
                .collect();

            let mut successes: Vec<String> = Vec::new();
            let mut exhausted = 0usize;
            let mut other_errs: Vec<String> = Vec::new();
            for h in handles {
                match h.join().unwrap() {
                    Ok(v) => successes.push(v["path"].as_str().unwrap().to_string()),
                    Err(e) if e.contains("pool exhausted") => exhausted += 1,
                    Err(e) => other_errs.push(e),
                }
            }
            assert!(
                other_errs.is_empty(),
                "iter {iter}: unexpected non-exhausted errors: {other_errs:?}"
            );
            assert_eq!(
                successes.len(),
                SLOTS,
                "iter {iter}: expected {SLOTS} successes, got {} (exhausted={exhausted})",
                successes.len()
            );
            assert_eq!(
                exhausted,
                THREADS - SLOTS,
                "iter {iter}: expected {} exhausted, got {exhausted}",
                THREADS - SLOTS
            );
            // Distinct-slot invariant: no two acquires landed on the
            // same slot path. This is the actual safety property —
            // double-claim would mean two threads both ran
            // `git checkout -b` in the same worktree.
            let mut sorted = successes.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                successes.len(),
                "iter {iter}: double-claim detected, paths: {successes:?}"
            );
        }
    }

    #[test]
    fn race_safety_lockfile_cleanup_release_reacquire_round_trips() {
        // Verifies the third invariant from the followup: each acquire
        // that succeeded → release → next iteration can re-acquire the
        // same slot. If pool_acquire ever leaked the /tmp lockfile
        // across iterations, every slot would be permanently contended
        // and round 2 would fail with `pool exhausted (... lost
        // lockfile race)`.
        let (_tmp, root) = fixture_with_pool(2);
        let mut paths_round_1: Vec<String> = Vec::new();
        for i in 0..5 {
            let r1 = pool_acquire(&root, Some(&format!("feat-rt-a-{i}")), None)
                .unwrap_or_else(|e| panic!("round {i} acquire-a failed: {e}"));
            let r2 = pool_acquire(&root, Some(&format!("feat-rt-b-{i}")), None)
                .unwrap_or_else(|e| panic!("round {i} acquire-b failed: {e}"));
            let p1 = r1["path"].as_str().unwrap().to_string();
            let p2 = r2["path"].as_str().unwrap().to_string();
            assert_ne!(p1, p2, "round {i}: same slot twice");
            if i == 0 {
                paths_round_1.push(p1.clone());
                paths_round_1.push(p2.clone());
            } else {
                // Same physical slots reused every round (set equality).
                let mut got = vec![p1.clone(), p2.clone()];
                got.sort();
                let mut want = paths_round_1.clone();
                want.sort();
                assert_eq!(got, want, "round {i}: slot set drifted");
            }
            pool_release(&root, Path::new(&p1), false).unwrap();
            pool_release(&root, Path::new(&p2), false).unwrap();
        }
    }

    #[test]
    fn validate_branch_name_rejects_bad_inputs() {
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("-leading-dash").is_err());
        assert!(validate_branch_name("foo..bar").is_err());
        assert!(validate_branch_name("foo bar").is_err());
        assert!(validate_branch_name("foo;rm").is_err());
        assert!(validate_branch_name("good-name_1.0/sub").is_ok());
    }

    #[test]
    fn acquire_with_explicit_base_sha_uses_it() {
        let (_tmp, root) = fixture_with_pool(1);
        let main_repo = Repository::open(&root).unwrap();
        let main_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let res =
            pool_acquire(&root, Some("feat-explicit-base"), Some(&main_tip)).expect("acquire ok");
        assert_eq!(res["base_sha"], main_tip);
    }

    #[test]
    fn acquire_rejects_unknown_base_sha() {
        let (_tmp, root) = fixture_with_pool(1);
        // Fake but valid-shape sha.
        let fake = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let err = pool_acquire(&root, Some("feat-bad-base"), Some(fake))
            .expect_err("must reject unknown sha");
        assert!(err.contains("not in repo"), "got {err:?}");
    }

    /// P0 atomic-acquire (2026-04-28): a slot whose detached HEAD is
    /// stale (sitting at an OLDER commit than current main) must
    /// produce a branch at *current* main when `pool_acquire` is
    /// called with `branch_name=Some, base_sha=None`. The load-bearing
    /// step is the `git checkout -b <branch> <resolved_base>` line
    /// that uses `resolved_base` (= current main tip) as the explicit
    /// start point, NOT the slot's pre-existing detached HEAD. If the
    /// `resolved_base` derivation is removed and pool_acquire instead
    /// runs `git checkout -b <branch>` (no explicit base), the new
    /// branch points at the slot's STALE detached HEAD, this test
    /// asserts the resulting branch tip equals current-main, and that
    /// assertion fails — the regression class the prompt names.
    ///
    /// Setup: 1-slot fixture starts detached at main-tip-1 (the seed
    /// commit). Add a SECOND commit on main (main-tip-2). Detach the
    /// slot back to main-tip-1 (simulating a stale slot that hasn't
    /// caught up to a recent main bump). pool_acquire with no base_sha
    /// → branch must point at main-tip-2, NOT main-tip-1.
    #[test]
    fn acquire_branches_at_current_main_not_stale_detached_head() {
        let (_tmp, root) = fixture_with_pool(1);
        let main_repo = Repository::open(&root).unwrap();
        let stale_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        // Advance main by one commit so current main != slot's
        // detached HEAD.
        let sig = Signature::now("t", "t@e.com").unwrap();
        commit_file(&main_repo, &sig, "advance.txt", "advance", "main: advance");
        let current_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        assert_ne!(
            stale_tip, current_tip,
            "fixture must produce two distinct main tips so stale-base path is exercised"
        );
        // Slot still detached at stale_tip from fixture_with_pool();
        // explicit re-detach to make the precondition load-bearing
        // (not relying on fixture internals).
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        {
            let slot_repo = Repository::open(&slot_path).unwrap();
            slot_repo
                .set_head_detached(git2::Oid::from_str(&stale_tip).unwrap())
                .unwrap();
        }
        // Acquire with no explicit base_sha → must resolve to current
        // main tip, NOT the slot's stale detached HEAD.
        let res = pool_acquire(&root, Some("feat-atomic-acquire"), None).expect("acquire ok");
        assert_eq!(
            res["base_sha"], current_tip,
            "pool_acquire must resolve base_sha to current main, got stale {stale_tip}"
        );
        // Post-condition: the slot's branch HEAD == current_tip, NOT
        // stale_tip. This is the assertion that fails when the rebase
        // step is removed: without `resolved_base` as explicit start
        // point, `git checkout -b feat-atomic-acquire` points the
        // branch at the slot's detached HEAD (= stale_tip).
        let slot_repo = Repository::open(&slot_path).unwrap();
        let branch_tip = slot_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        assert_eq!(
            branch_tip, current_tip,
            "slot HEAD must be current main, got stale-detached value {branch_tip}"
        );
        // Branch name confirms (b) from the spec.
        assert_eq!(res["branch"], "feat-atomic-acquire");
    }

    /// Backward-compat path (2026-04-28): pool_acquire with
    /// `branch_name=None` returns a free slot still detached at the
    /// resolved base sha (current main when base_sha is None). No
    /// branch is created. Caller is then free to do their own branch
    /// dance (e.g. checkout an existing branch, multi-step git
    /// operation). Response payload omits `branch` field.
    #[test]
    fn acquire_without_branch_name_returns_detached_slot() {
        let (_tmp, root) = fixture_with_pool(1);
        let main_repo = Repository::open(&root).unwrap();
        let main_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let res = pool_acquire(&root, None, None).expect("detached acquire ok");
        assert_eq!(res["base_sha"], main_tip);
        assert!(
            res.get("branch").is_none(),
            "branch field must be absent when caller didn't request one, got {res}"
        );
        // The slot is still detached.
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        let slot_repo = Repository::open(&slot_path).unwrap();
        assert!(
            !slot_repo.head().unwrap().is_branch(),
            "slot must be detached when no branch_name supplied"
        );
        let slot_tip = slot_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        assert_eq!(slot_tip, main_tip);
    }

    /// Detached path also drives the slot forward when its pre-existing
    /// HEAD is stale relative to current main. Same regression class as
    /// the branch-creation path, but for the no-branch-name caller.
    #[test]
    fn acquire_detached_advances_stale_head_to_current_main() {
        let (_tmp, root) = fixture_with_pool(1);
        let main_repo = Repository::open(&root).unwrap();
        let stale_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let sig = Signature::now("t", "t@e.com").unwrap();
        commit_file(&main_repo, &sig, "advance.txt", "advance", "main: advance");
        let current_tip = main_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        {
            let slot_repo = Repository::open(&slot_path).unwrap();
            slot_repo
                .set_head_detached(git2::Oid::from_str(&stale_tip).unwrap())
                .unwrap();
        }
        let res = pool_acquire(&root, None, None).expect("detached acquire ok");
        assert_eq!(res["base_sha"], current_tip);
        let slot_repo = Repository::open(&slot_path).unwrap();
        let slot_tip = slot_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        assert_eq!(
            slot_tip, current_tip,
            "detached acquire must advance stale slot to current main"
        );
    }

    /// MED-PR1 regression: detached HEAD with a dirty working tree
    /// must NOT bypass the release gate. Pre-fix, the only refusal
    /// was inside `if head.is_branch() && !force`, so a detached
    /// recovery-path slot with edits was reset --hard'd silently.
    /// Post-fix, the always-on dirty check refuses; force=true still
    /// overrides for callers that genuinely want a hard reset.
    #[test]
    fn release_refuses_detached_dirty_without_force() {
        let (_tmp, root) = fixture_with_pool(1);
        // Slot starts detached at main-tip per fixture_with_pool().
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        // Sanity: confirm the slot is detached (not on a branch).
        let slot_repo = Repository::open(&slot_path).unwrap();
        assert!(
            !slot_repo.head().unwrap().is_branch(),
            "fixture must leave slot detached so we exercise the new gate"
        );
        // Drop a dirty file (untracked) into the slot — the prior
        // gate ignored this entirely because head.is_branch() was
        // false.
        fs::write(slot_path.join("uncommitted.txt"), "would be discarded").unwrap();
        // Release without force → must refuse.
        let err = pool_release(&root, &slot_path, false)
            .expect_err("dirty detached HEAD must refuse without force");
        assert!(err.contains("working tree dirty"), "got {err:?}");
        assert!(err.contains("force=true"), "got {err:?}");
        // File still present (gate fired before reset --hard).
        assert!(
            slot_path.join("uncommitted.txt").exists(),
            "refusal must be a no-op; file should not have been wiped"
        );
        // Release with force=true → must succeed and clean the file.
        let rel = pool_release(&root, &slot_path, true)
            .expect("force release must succeed on dirty slot");
        assert_eq!(rel["released"], true);
        assert!(
            !slot_path.join("uncommitted.txt").exists(),
            "force release must wipe the dirty file"
        );
    }

    /// P0 mutation probe (2026-04-28): pool_release MUST drop ignored
    /// files (e.g. `target/junk`) on release, otherwise classify_slot
    /// keeps the slot in the `free` set (its is_dirty() doesn't see
    /// ignored files) AND the next pool_acquire hands the slot out
    /// with the prior session's build cruft in place.
    ///
    /// This is a true mutation probe: replacing the production line
    ///     git clean -fdx -e .cargo
    /// with the prior
    ///     git clean -fd
    /// causes the `target/junk` assertion below to fail (file
    /// persists after release because `-fd` does not remove ignored
    /// files). That is the regression class the prompt names.
    ///
    /// Setup: gitignore exists in the repo with `target/` ignored;
    /// the slot's branch creates a `target/junk` (ignored) and a
    /// `top.txt` (untracked, would be wiped by either `-fd` or `-fdx`).
    /// Release with force=true (so the dirty refusal doesn't fire).
    /// Post-condition: BOTH files gone.
    #[test]
    fn release_clean_fdx_drops_ignored_files() {
        let (_tmp, root) = fixture_with_pool(1);
        // Bake a .gitignore that ignores target/ on the main commit so
        // the slot's `target/` is genuinely "ignored" (vs untracked).
        let main_repo = Repository::open(&root).unwrap();
        let sig = Signature::now("t", "t@e.com").unwrap();
        commit_file(
            &main_repo,
            &sig,
            ".gitignore",
            "target/\n",
            "main: gitignore",
        );
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        // Move slot to current main so .gitignore is present.
        let main_tip = main_repo.head().unwrap().peel_to_commit().unwrap().id();
        {
            let slot_repo = Repository::open(&slot_path).unwrap();
            slot_repo.set_head_detached(main_tip).unwrap();
            // Hard-checkout so working tree matches.
            let mut co = git2::build::CheckoutBuilder::new();
            co.force();
            slot_repo.checkout_head(Some(&mut co)).unwrap();
        }
        // Drop an ignored file (target/junk) into the slot first +
        // sanity-check is_dirty does NOT see it. This is the load-
        // bearing observation: classify_slot's is_dirty() doesn't see
        // ignored files (default StatusOptions excludes them), so a
        // slot holding only ignored leftovers stays "free" and the
        // next pool_acquire hands it out with prior-session state.
        fs::create_dir_all(slot_path.join("target")).unwrap();
        fs::write(slot_path.join("target").join("junk"), "build cruft").unwrap();
        {
            let slot_repo = Repository::open(&slot_path).unwrap();
            assert!(
                !is_dirty(&slot_repo).unwrap(),
                "ignored target/junk must NOT mark slot dirty (the load-bearing premise)"
            );
        }
        // Now drop an untracked file too. is_dirty WILL see this
        // (top.txt is untracked, not ignored), so release without
        // force would refuse. Use force=true to exercise the clean
        // step independently of the refusal gate.
        fs::write(slot_path.join("top.txt"), "untracked").unwrap();
        let rel = pool_release(&root, &slot_path, true).expect("force release ok");
        assert_eq!(rel["released"], true);
        // Both files must be gone — target/junk is the mutation probe
        // (only `-fdx` reaches it), top.txt is the regular untracked
        // case (`-fd` would also reach it).
        assert!(
            !slot_path.join("target").join("junk").exists(),
            "release must drop ignored target/junk (mutation probe — \
             swap `-fdx -e .cargo` for `-fd` and this assertion fails)"
        );
        assert!(
            !slot_path.join("top.txt").exists(),
            "release must drop untracked top.txt"
        );
    }

    /// `.cargo/config.toml` (worktree-template-copied per
    /// `tools/worktree-template/.cargo/config.toml`) is *untracked* in
    /// pool slots — `-fd` alone would wipe it, breaking the per-
    /// worktree `target-dir = "./target"` override on the next
    /// acquire. The `-e .cargo` exempt preserves it.
    ///
    /// Mutation probe: drop the `-e .cargo` argument and this test
    /// fails (`.cargo/config.toml` deleted by clean step).
    #[test]
    fn release_preserves_worktree_template_cargo_config() {
        let (_tmp, root) = fixture_with_pool(1);
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        // Simulate worktree-create.sh's template copy: drop a
        // `.cargo/config.toml` into the slot. It's untracked.
        fs::create_dir_all(slot_path.join(".cargo")).unwrap();
        fs::write(
            slot_path.join(".cargo").join("config.toml"),
            "[build]\ntarget-dir = \"./target\"\n",
        )
        .unwrap();
        // Drop a non-exempt untracked file too so we have something
        // for clean -fdx to actually remove (so we know the clean
        // step ran rather than no-op'd).
        fs::write(slot_path.join("scratch.txt"), "scratch").unwrap();
        // Force-release (untracked → dirty → refusal without force).
        let rel = pool_release(&root, &slot_path, true).expect("force release ok");
        assert_eq!(rel["released"], true);
        // `.cargo/config.toml` must survive (this is the load-bearing
        // assertion — `-e .cargo` is what makes it survive).
        assert!(
            slot_path.join(".cargo").join("config.toml").exists(),
            "release must preserve worktree-template .cargo/config.toml \
             (mutation probe — drop `-e .cargo` and this fails)"
        );
        // The non-exempt scratch file must be gone (so we know clean
        // ran).
        assert!(
            !slot_path.join("scratch.txt").exists(),
            "non-exempt untracked file must be removed by clean step"
        );
    }

    /// End-to-end contract assertion (NOT a mutation probe of the
    /// verify block): after `pool_release` returns Ok, the slot is
    /// detached at main_tip AND `git status --porcelain` empty.
    /// Independently re-verifies state via Repository::open so a
    /// regression in any of detach / reset / clean / verify shows up
    /// as a test failure here. For a focused mutation probe of the
    /// `verify_post_release` helper, see the three `verify_post_release_*`
    /// tests below.
    #[test]
    fn release_postcondition_contract_redundantly_verifies_state() {
        let (_tmp, root) = fixture_with_pool(1);
        let res = pool_acquire(&root, Some("feat-postcond"), None).expect("acquire ok");
        let path = res["path"].as_str().unwrap();
        let rel = pool_release(&root, Path::new(path), false).expect("release ok");
        assert_eq!(rel["released"], true);
        let slot_repo = Repository::open(path).unwrap();
        let head = slot_repo.head().unwrap();
        assert!(
            !head.is_branch(),
            "post-release HEAD must be detached, not on a branch"
        );
        let main_repo = Repository::open(&root).unwrap();
        let main_tip = main_repo.head().unwrap().peel_to_commit().unwrap().id();
        let slot_tip = head.peel_to_commit().unwrap().id();
        assert_eq!(slot_tip, main_tip, "post-release HEAD must equal main_tip");
        assert!(
            !is_dirty(&slot_repo).unwrap(),
            "post-release working tree must be clean"
        );
    }

    /// Mutation probe of `verify_post_release` (Rule 11 deletion-
    /// counterfactual): construct a torn state where HEAD is on a
    /// branch (detach step elided), then call `verify_post_release`
    /// directly. Must Err with "HEAD is still on a branch". Drop the
    /// `if post_head.is_branch()` block in `verify_post_release` →
    /// this test fails (Err becomes Ok).
    #[test]
    fn verify_post_release_rejects_head_on_branch() {
        let (_tmp, root) = fixture_with_pool(1);
        let res = pool_acquire(&root, Some("feat-torn-branch"), None).expect("acquire ok");
        let path = PathBuf::from(res["path"].as_str().unwrap());
        // Slot HEAD is now on `feat-torn-branch`. Don't release —
        // call verify directly to probe the branch-rejection path.
        let main_repo = Repository::open(&root).unwrap();
        let main_tip = main_repo.head().unwrap().peel_to_commit().unwrap().id();
        let err =
            verify_post_release(&path, main_tip).expect_err("verify must reject HEAD-on-branch");
        assert!(
            err.contains("HEAD is still on a branch"),
            "expected branch-reject error, got: {err}"
        );
    }

    /// Mutation probe of `verify_post_release` (Rule 11): construct a
    /// detached slot at a non-main commit, call verify with main_tip
    /// matching the actual main HEAD. Must Err with "HEAD ... !=
    /// main_tip". Drop the `if post_tip != main_tip` block → this
    /// test fails.
    #[test]
    fn verify_post_release_rejects_head_at_wrong_sha() {
        let (_tmp, root) = fixture_with_pool(1);
        let res = pool_acquire(&root, None, None).expect("acquire ok");
        let path = PathBuf::from(res["path"].as_str().unwrap());
        // Slot is detached at main_tip. Add a commit to main so
        // main_tip diverges from the slot's HEAD.
        let main_repo = Repository::open(&root).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap();
        commit_file(&main_repo, &sig, "extra.txt", "extra", "main: extra");
        let new_main_tip = main_repo.head().unwrap().peel_to_commit().unwrap().id();
        let err = verify_post_release(&path, new_main_tip)
            .expect_err("verify must reject HEAD != main_tip");
        assert!(
            err.contains("!= main_tip"),
            "expected sha-mismatch error, got: {err}"
        );
    }

    /// Mutation probe of `verify_post_release` (Rule 11): construct a
    /// detached slot at main_tip but with a dirty tracked file, call
    /// verify. Must Err with "working tree still dirty". Drop the
    /// `if is_dirty(&post_repo)?` block → this test fails.
    #[test]
    fn verify_post_release_rejects_dirty_tree() {
        let (_tmp, root) = fixture_with_pool(1);
        let res = pool_acquire(&root, None, None).expect("acquire ok");
        let path = PathBuf::from(res["path"].as_str().unwrap());
        // Slot is detached at main_tip. Dirty the tree by writing an
        // untracked file (not under .cargo/, which is_dirty filters).
        std::fs::write(path.join("dirty-marker.txt"), "torn").unwrap();
        let main_repo = Repository::open(&root).unwrap();
        let main_tip = main_repo.head().unwrap().peel_to_commit().unwrap().id();
        let err = verify_post_release(&path, main_tip).expect_err("verify must reject dirty tree");
        assert!(
            err.contains("working tree still dirty"),
            "expected dirty-tree error, got: {err}"
        );
    }

    /// LOW-PR2 (2026-04-28) — crash-resilience contract.
    ///
    /// Simulates a crashed prior holder by leaving a stale lockfile on
    /// disk WITHOUT any process holding flock on it. Pre-fix
    /// (`OpenOptions::create_new`/O_EXCL marker scheme), the next
    /// `pool_acquire` call would observe the marker file existing,
    /// fail `create_new`, fall through every free slot, and return
    /// `pool exhausted (... lost lockfile race)`. Post-fix (flock-
    /// based locking), the file's existence is irrelevant — `create`
    /// (without `_new`) opens it idempotently, `try_lock_exclusive`
    /// succeeds because no live FD holds the lock, and the acquire
    /// proceeds normally.
    ///
    /// Mutation probe: revert `lock_path_for`'s open call back to
    /// `create_new(true)` (the pre-fix shape) and this test fails
    /// — `pool_acquire` returns `pool exhausted` with `lost lockfile
    /// race` in the error string instead of succeeding.
    #[test]
    fn acquire_recovers_from_stale_lockfile_without_live_holder() {
        let (_tmp, root) = fixture_with_pool(1);
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        // Pre-create the lockfile with no live holder — emulates a
        // process that crashed mid-acquire and left the marker on
        // disk. Note: nothing flocks this; just the file existing
        // is the simulated stale state.
        let lock_path = lock_path_for(&slot_path);
        fs::write(&lock_path, b"stale from crashed pid 12345").unwrap();
        assert!(
            lock_path.exists(),
            "precondition: stale lockfile must be on disk"
        );
        // pool_acquire must succeed despite the stale lockfile.
        let res = pool_acquire(&root, Some("feat-stale-recover"), None)
            .expect("acquire must recover from stale lockfile");
        assert_eq!(res["branch"], "feat-stale-recover");
        assert_eq!(res["path"].as_str().unwrap(), slot_path.to_str().unwrap());
    }

    /// LOW-PR2 — contention contract: a live process holding flock on
    /// a slot's lockfile must cause concurrent acquires to skip that
    /// slot WITHOUT panicking. Different from the existing race
    /// tests, which exercise two acquires both reaching
    /// `try_lock_exclusive` from inside `pool_acquire`. Here we
    /// emulate an out-of-band holder (e.g. another wtpool
    /// process) by holding flock on the marker file from inside this
    /// test, then call `pool_acquire` and observe it falls through to
    /// the next free slot.
    ///
    /// 2-slot fixture: slot 1 is held; pool_acquire must land on
    /// slot 2 and report success.
    #[test]
    fn acquire_skips_slot_with_live_flock_holder() {
        let (_tmp, root) = fixture_with_pool(2);
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_1 = pool.join("wt-01");
        let slot_2 = pool.join("wt-02");
        // Take the flock on slot 1's lockfile and HOLD IT for the
        // duration of the acquire call. Use the same path
        // pool_acquire would compute.
        let lock_path_slot_1 = lock_path_for(&slot_1);
        let holder = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path_slot_1)
            .unwrap();
        FileExt::try_lock_exclusive(&holder).expect("test holder must take lock");
        // pool_acquire must skip slot 1 (lock contended) and land on
        // slot 2. NOT panic, NOT return pool-exhausted.
        let res =
            pool_acquire(&root, Some("feat-contention"), None).expect("acquire must skip slot 1");
        assert_eq!(
            res["path"].as_str().unwrap(),
            slot_2.to_str().unwrap(),
            "acquire must land on slot 2; held lock on slot 1"
        );
        // Drop the holder → flock released. Sanity: the lockfile
        // path still exists on disk (we don't remove it on drop).
        drop(holder);
        assert!(
            lock_path_slot_1.exists(),
            "lockfile must persist after release (flock semantics, not O_EXCL)"
        );
    }

    /// LOW-PR2 — auto-release contract: a holder that drops its lock
    /// FD without explicit `unlock` (simulating crash-style abrupt
    /// exit, since `Drop`-on-`File` triggers the same close(2) the
    /// kernel sees on process death) must permit the next acquire
    /// to take the lock immediately. This is the core kernel-managed
    /// contract: flock release is tied to FD close, not to graceful
    /// shutdown. With the pre-fix O_EXCL scheme the equivalent test
    /// would require an explicit `remove_file` cleanup step in the
    /// holder; the absence of that cleanup is exactly the failure
    /// mode LOW-PR2 names.
    #[test]
    fn acquire_succeeds_after_holder_fd_close_simulates_crash() {
        let (_tmp, root) = fixture_with_pool(1);
        let pool = root.join(".claude").join("worktrees").join("pool");
        let slot_path = pool.join("wt-01");
        let lock_path = lock_path_for(&slot_path);
        // Holder block: take the lock, then let the File go out of
        // scope. No explicit unlock; this is what a crashed process
        // looks like to the kernel (FD closed via process teardown
        // → flock released).
        {
            let holder = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            FileExt::try_lock_exclusive(&holder).expect("holder takes lock");
            // No explicit `holder.unlock()`; rely on Drop closing
            // the FD, which is what the kernel sees on crash.
        }
        // Lockfile still exists on disk (would fail an O_EXCL-based
        // probe), but no live holder.
        assert!(lock_path.exists(), "lockfile persists after holder drop");
        let res = pool_acquire(&root, Some("feat-post-crash"), None)
            .expect("acquire must succeed after holder FD close");
        assert_eq!(res["branch"], "feat-post-crash");
    }
}
