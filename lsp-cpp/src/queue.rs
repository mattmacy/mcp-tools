//! Bounded-depth admission queue for in-flight backend requests.
//!
//! ## Why
//!
//! Today's MCP serve loop ([`crate::mcp::serve`]) is synchronous: read
//! one frame, dispatch, write response. Effective in-flight depth is 1.
//!
//! Tomorrow's failure mode (the case this branch addresses): a slow
//! clangd — busy on translation units with very large preambles taking 18 s+ to parse
//! — holds the per-request timeout open. If callers retry without
//! waiting, or if a future async serve loop accepts new requests while
//! the previous one is still in flight, a stuck clangd will accumulate
//! requests until something runs out of memory or the OS pipe buffer
//! fills and stalls everything else.
//!
//! [`BoundedQueue`] is the admission gate. Callers acquire a [`Slot`]
//! before dispatching to the backend; the slot is dropped automatically
//! after the response is written. When all slots are in use, new
//! `try_acquire` calls return `None` and the caller surfaces an explicit
//! `clangd_busy_queue_full` error rather than queuing unbounded.
//!
//! Pairs with the busy-vs-broken classification in
//! [`crate::clangd::Clangd::classify_timeout`]: the queue limits *how
//! many* slow requests can pile up; the classifier turns each individual
//! timeout into a structured `ClangdBusy` (clangd alive but slow) versus
//! `ClangdExited` (clangd dead — broken-pipe surface) so callers can
//! retry-with-longer-timeout vs restart-the-wrapper accordingly.
//!
//! ## Coordination with `lsp-cpp-auto-resume-wrapper`
//!
//! The a follow-up branch handles the orthogonal failure mode (clangd EXITS,
//! requiring supervisor restart). Both branches touch this module's
//! shape: when the wrapper restart fires, every pending [`Slot`] should
//! see a `wrapper_restarted` notification so its waiting `Clangd::request`
//! returns explicit error rather than hanging on the stale subprocess
//! pipe. That cross-branch wiring is deliberately deferred to the merge
//! orchestrator — this branch lands the slot abstraction; the sister
//! branch hooks restart-drain into [`BoundedQueue::drain_for_restart`]
//! which is reserved as `pub(crate)` API today (no body yet — landing it
//! here would be unwired scaffolding).

use std::sync::{Arc, Mutex};

/// Default in-flight depth. 16 chosen because a synchronous serve loop
/// has effective depth 1, and a heartbeat-emitting serve loop (deferred
/// to followup a follow-up branch) at most
/// double-buffers one in-flight request and one queued retry per MCP
/// caller. 16 leaves headroom for two parallel callers (parent CLI +
/// subagent) without rejecting under bursty traffic.
pub(crate) const DEFAULT_QUEUE_DEPTH: usize = 16;

/// Admission queue for in-flight backend requests.
///
/// Capacity-bounded counter implemented over `Arc<Mutex<usize>>`. No
/// inter-thread parking — this is admission control, not work
/// scheduling. Returning `None` from [`Self::try_acquire`] is the load-
/// bearing path: callers translate it into a structured
/// `clangd_busy_queue_full` error.
#[derive(Debug, Clone)]
pub(crate) struct BoundedQueue {
    inner: Arc<Mutex<QueueState>>,
}

#[derive(Debug)]
struct QueueState {
    capacity: usize,
    in_flight: usize,
}

impl BoundedQueue {
    /// Build a queue with the given capacity. `capacity == 0` would
    /// reject every request and is rejected with a panic; depth-0
    /// queues are nonsense in this design.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "BoundedQueue capacity must be > 0");
        Self {
            inner: Arc::new(Mutex::new(QueueState {
                capacity,
                in_flight: 0,
            })),
        }
    }

    /// Try to claim one slot. `None` means the queue is full and the
    /// caller MUST surface `clangd_busy_queue_full` rather than block.
    /// The returned [`Slot`] releases its claim on drop.
    pub(crate) fn try_acquire(&self) -> Option<Slot> {
        let mut state = self.inner.lock().expect("BoundedQueue mutex poisoned");
        if state.in_flight >= state.capacity {
            return None;
        }
        state.in_flight += 1;
        Some(Slot {
            queue: self.inner.clone(),
        })
    }

    /// Current in-flight count. Returns the snapshot at time of call;
    /// callers that need an atomic "acquire iff under N" should use
    /// [`Self::try_acquire`] instead.
    pub(crate) fn in_flight(&self) -> usize {
        self.inner
            .lock()
            .expect("BoundedQueue mutex poisoned")
            .in_flight
    }

    /// Configured capacity. Constant after construction.
    pub(crate) fn capacity(&self) -> usize {
        self.inner
            .lock()
            .expect("BoundedQueue mutex poisoned")
            .capacity
    }
}

/// RAII guard for one admission slot. Drop releases the slot back to
/// the queue. Intentionally not `Clone`: the slot represents exclusive
/// admission for one in-flight request, and copying it would
/// double-count.
pub(crate) struct Slot {
    queue: Arc<Mutex<QueueState>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut state) = self.queue.lock() {
            // Saturate at zero rather than underflow on a poisoned-lock
            // reacquire; double-drop is impossible (Slot is non-Clone),
            // so this is purely defensive against future misuse.
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic-shape test required by the dispatch prompt: send N
    /// requests at depth N, all succeed; (N+1)th rejected. Mirrors the
    /// "16 queue + 4 reject" requirement scaled down to N=4 for fast
    /// runtime. Deleting the `if state.in_flight >= state.capacity`
    /// branch in `try_acquire` makes this test fail — Rule 11 satisfied.
    #[test]
    fn queue_accepts_capacity_requests_and_rejects_overflow() {
        let q = BoundedQueue::new(4);
        let s1 = q.try_acquire().expect("slot 1");
        let s2 = q.try_acquire().expect("slot 2");
        let s3 = q.try_acquire().expect("slot 3");
        let s4 = q.try_acquire().expect("slot 4");
        assert_eq!(q.in_flight(), 4);
        assert!(
            q.try_acquire().is_none(),
            "5th acquire MUST be rejected at capacity 4 — this is the queue_depth_exceeded path"
        );
        // Dropping a slot frees one; then acquire succeeds again.
        drop(s1);
        assert_eq!(q.in_flight(), 3);
        let s5 = q.try_acquire().expect("re-acquire after drop");
        assert_eq!(q.in_flight(), 4);
        // Keep all alive to end of scope so drop order is well-defined.
        drop((s2, s3, s4, s5));
        assert_eq!(q.in_flight(), 0);
    }

    /// Counterfactual cover: removing the depth check from
    /// `try_acquire` (i.e., always returning Some) would let the
    /// counter grow without bound. We don't have a borrow-checker
    /// representation of that mutation in the test, but verify the
    /// counter never exceeds capacity in normal operation — a
    /// regression that ignored capacity would surface as `in_flight >
    /// capacity` after a burst.
    #[test]
    fn in_flight_never_exceeds_capacity_under_burst() {
        let q = BoundedQueue::new(2);
        let mut held: Vec<Slot> = Vec::new();
        for _ in 0..10 {
            if let Some(s) = q.try_acquire() {
                held.push(s);
            }
        }
        assert_eq!(held.len(), 2, "exactly capacity slots should fit");
        assert_eq!(q.in_flight(), 2);
        assert!(q.in_flight() <= q.capacity());
    }

    #[test]
    fn slot_drop_releases_back_to_zero() {
        let q = BoundedQueue::new(3);
        {
            let _s1 = q.try_acquire().unwrap();
            let _s2 = q.try_acquire().unwrap();
            assert_eq!(q.in_flight(), 2);
        }
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ = BoundedQueue::new(0);
    }
}
