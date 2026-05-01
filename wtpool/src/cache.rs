//! 60-second TTL cache for tool invocations.
//!
//! Every tool call is keyed by `(tool_name, canonical_args_json)`; the
//! cached `serde_json::Value` is reused if its insertion timestamp is
//! within the TTL window. Cache miss / expiry → caller recomputes,
//! reinserts. TTL-only — there is no inotify-driven invalidation, so a
//! freshly-created worktree may not appear in `worktree_list` for up to
//! 60 s. Spec §1.5 accepts that.
//!
//! The cache is `Mutex<HashMap>` not `RwLock` because every tool call
//! eventually ends up writing (insert-on-miss, prune-on-expiry); the
//! contention budget is one MCP loop, not many readers + few writers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Default TTL for all cached tool invocations. Spec §1.5: 60 s.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Single cache entry: (insertion-time, payload).
struct Entry {
    inserted_at: Instant,
    value: Value,
}

/// Mutex-guarded TTL cache shared across MCP tool handlers. The `Mutex`
/// is deliberate (rather than `RwLock`) — hits write back the entry's
/// last-read timestamp implicitly via insertion-only TTL, but every
/// miss path takes a write lock to insert. RwLock would only complicate
/// the API for negligible contention savings on a single-client stdio
/// server.
pub struct TtlCache {
    map: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
}

impl TtlCache {
    /// Construct a cache with the spec-default 60 s TTL.
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }

    /// Construct a cache with a custom TTL. Used by tests to
    /// deterministically force expiry without sleeping for 60 s.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Compute-or-cache. Calls `f` only on miss / expiry; returns the
    /// (cached or freshly-computed) `Value`. Errors from `f` are
    /// propagated and NOT cached — failed calls re-evaluate next time.
    pub fn get_or_compute<F>(&self, key: &str, f: F) -> Result<Value, String>
    where
        F: FnOnce() -> Result<Value, String>,
    {
        // Read-side check.
        if let Some(value) = self.peek(key) {
            return Ok(value);
        }
        // Recompute outside the lock to avoid holding the mutex across
        // a potentially-slow git2 call. This permits a brief window
        // where two callers race + both compute, but on a single-client
        // stdio server that's free.
        let value = f()?;
        let mut guard = self.map.lock().expect("cache mutex poisoned");
        guard.insert(
            key.to_string(),
            Entry {
                inserted_at: Instant::now(),
                value: value.clone(),
            },
        );
        Ok(value)
    }

    /// Read a fresh entry without recomputing. Returns `None` on miss
    /// or if the entry has expired.
    pub fn peek(&self, key: &str) -> Option<Value> {
        let guard = self.map.lock().expect("cache mutex poisoned");
        let entry = guard.get(key)?;
        if entry.inserted_at.elapsed() > self.ttl {
            None
        } else {
            Some(entry.value.clone())
        }
    }

    /// Drop all entries. Test-only convenience; not exposed to MCP
    /// tools because the spec'd cache is TTL-only.
    #[cfg(test)]
    pub fn clear(&self) {
        self.map.lock().expect("cache mutex poisoned").clear();
    }
}

impl Default for TtlCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn cache_hit_avoids_recompute() {
        let cache = TtlCache::new();
        let calls = AtomicU32::new(0);
        let f = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"x": 1}))
        };
        let v1 = cache.get_or_compute("k", f).unwrap();
        let g = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"x": 1}))
        };
        let v2 = cache.get_or_compute("k", g).unwrap();
        assert_eq!(v1, v2);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "second call must hit");
    }

    #[test]
    fn cache_expiry_forces_recompute() {
        let cache = TtlCache::with_ttl(Duration::from_millis(20));
        let calls = AtomicU32::new(0);
        cache
            .get_or_compute("k", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"x": 1}))
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        cache
            .get_or_compute("k", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"x": 2}))
            })
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "post-expiry call must recompute"
        );
    }

    #[test]
    fn cache_distinct_keys_do_not_alias() {
        let cache = TtlCache::new();
        cache.get_or_compute("a", || Ok(json!(1))).unwrap();
        cache.get_or_compute("b", || Ok(json!(2))).unwrap();
        assert_eq!(cache.peek("a"), Some(json!(1)));
        assert_eq!(cache.peek("b"), Some(json!(2)));
    }

    #[test]
    fn cache_error_not_persisted() {
        let cache = TtlCache::new();
        let calls = AtomicU32::new(0);
        let _ = cache.get_or_compute::<_>("k", || -> Result<Value, String> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("transient".into())
        });
        // Subsequent call must re-run, not return cached error.
        let _ = cache.get_or_compute::<_>("k", || -> Result<Value, String> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!("ok"))
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.peek("k"), Some(json!("ok")));
    }
}
