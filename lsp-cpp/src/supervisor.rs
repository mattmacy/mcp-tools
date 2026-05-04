//! Auto-resume / supervisor state machine for the long-lived clangd
//! subprocess that backs the MCP transport.
//!
//! ## Why this exists — 2026-04-27 ~17:30 Z incident anchor
//!
//! Today clangd zombified mid-session under heavy UMG indexing
//! (100 MB preambles, 18 s build time per TU). The shim's blocking
//! `request()` call hit a broken-pipe `io::Error` because the dead child's
//! stdout had closed. The MCP transport surfaced this as
//! `ShimError::Io(...)` with no recovery path. The operator (the
//! workspace's primary CC session) then sent `SIGHUP` to the wrapper
//! process to "reset" it, which terminated the wrapper itself rather
//! than the child clangd. CC's stdio MCP transport does not auto-respawn
//! a dead `command`-style server — once the wrapper PID is gone, the
//! `mcp__lsp-cpp__*` tools are unrecoverable until the *whole CC
//! session* restarts (a multi-minute interruption that drops in-flight
//! agent transcripts on the floor).
//!
//! The cure is in two halves:
//!
//! 1. **Supervisor (this module).** The wrapper holds the clangd child
//!    behind a supervisor that:
//!    - detects a dead child at every MCP-request boundary
//!      (`Child::try_wait()`),
//!    - respawns it transparently on the *next* request rather than
//!      surfacing the crash to the model,
//!    - applies exponential backoff (1 s -> 2 s -> 4 s -> 8 s -> 16 s
//!      cap) so a wedge-then-respawn-then-wedge loop does not pin a
//!      core,
//!    - resets backoff after a 60 s healthy uptime window so the next
//!      crash starts at the bottom of the curve again,
//!    - fail-loud after 5 restarts in a 5 min sliding window with a
//!      structured `supervisor_max_retries` error so the model sees a
//!      *real* failure (not a fake "broken pipe" that hides the loop).
//!
//! 2. **Status RPC.** A new `lsp_cpp_status` MCP tool surfaces the
//!    supervisor's current state (`{clangd_pid, uptime_s, restart_count,
//!    last_restart_reason}`) so the operator can `tools/call` it instead
//!    of reaching for `kill -SIGHUP` to "see if the wrapper is alive."
//!    That single observability gap is what produced today's
//!    operator-error escalation.
//!
//! ## State machine
//!
//! ```text
//!     +---------+   spawn ok   +---------+
//!     | Stopped | ───────────▶ | Running |
//!     +---------+              +─────────+
//!          ▲                    │ child exit detected
//!          │                    ▼
//!          │  next request    +-----------+   restarts < cap   +---------+
//!          ╰───────────────── | Backoff(d)| ─────────────────▶ | Running |
//!                             +-----------+                    +---------+
//!                                   │ restarts >= cap in window
//!                                   ▼
//!                             +-----------+
//!                             |  Failed   | (returns supervisor_max_retries)
//!                             +-----------+ until window expires
//! ```
//!
//! `Backoff(d)` carries the next sleep duration. `Failed` is a soft
//! state — once the 5 min sliding window expires, the next request
//! reverts to `Stopped` and the supervisor retries from scratch. This
//! mirrors the way operating-system service managers (systemd, runit)
//! handle "rate-limited" service restarts: they do not give up forever,
//! they give up *for a window* so transient noise (one bad TU parse) does
//! not poison the host indefinitely.
//!
//! ## Why a separate module
//!
//! The state machine is pure data: counters, timestamps, decisions. No
//! I/O, no subprocess handles, no cargo-feature gating. The unit tests in
//! this file drive it via a mock clock (`SupervisorPolicy::observe_*`
//! takes an explicit `Instant`-equivalent) so a `kill -9 clangd` smoke
//! test is not required to validate the backoff curve, the max-retry
//! window, or the reset-after-healthy-uptime branch.
//!
//! The `Clangd` integration is intentionally narrow: the supervisor
//! exposes [`SupervisorPolicy::should_retry`] / [`record_spawn`] /
//! [`record_exit`] which the MCP loop calls at request boundaries.
//! Keeping the policy data-only also means a future async rewrite (the
//! a follow-up branch plans tokio-based
//! request multiplexing) reuses the policy verbatim and only swaps the
//! integration site.

use std::time::Duration;

/// Initial backoff after the first crash. Doubled on each subsequent
/// crash up to [`MAX_BACKOFF`].
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Cap on the exponential backoff. 16 s matches the 5-restart-in-5-min
/// budget — once we've slept 1 + 2 + 4 + 8 + 16 = 31 s we're already
/// over half the window, so further doubling is pointless.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(16);

/// A successful uptime of at least this long resets the backoff curve
/// to [`INITIAL_BACKOFF`]. Without this, a daily transient (one bad
/// TU parse 18 hours after spawn) would inherit the previous day's
/// `MAX_BACKOFF` even though the supervisor is functionally healthy.
pub(crate) const HEALTHY_UPTIME_RESET: Duration = Duration::from_secs(60);

/// Sliding window for the max-restart counter. Five restarts inside this
/// window trips [`SupervisorState::Failed`].
pub(crate) const RESTART_WINDOW: Duration = Duration::from_secs(300);

/// Maximum restarts allowed inside [`RESTART_WINDOW`] before
/// [`SupervisorPolicy::should_retry`] starts returning [`RetryDecision::Fail`].
pub(crate) const MAX_RESTARTS_IN_WINDOW: usize = 5;

/// Logical state of the supervised clangd. The MCP loop inspects this
/// at every request boundary via [`SupervisorPolicy::current_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SupervisorState {
    /// No child has been spawned yet (initial state and post-`Failed`
    /// reset state).
    Stopped,
    /// Child spawned successfully and the supervisor believes it is
    /// alive. The MCP loop is responsible for transitioning out of this
    /// state when it detects a dead child via `Child::try_wait()`; the
    /// supervisor itself holds no subprocess handle.
    Running,
    /// Child exited; the supervisor is sleeping `duration` before the
    /// next respawn attempt.
    Backoff {
        /// How long the next spawn attempt should wait before firing.
        duration: Duration,
    },
    /// Restart budget exhausted inside [`RESTART_WINDOW`]. The MCP
    /// loop returns a structured error with `error_kind =
    /// supervisor_max_retries` until the window slides past the
    /// oldest restart timestamp.
    Failed,
}

/// Why the most recent restart fired. Surfaced through the
/// `lsp_cpp_status` tool so an operator can distinguish a
/// build-induced crash (e.g., UMG indexing) from a binary-not-found
/// configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestartReason {
    /// First spawn or reset after `Failed` state expired.
    InitialSpawn,
    /// Child exited cleanly with a non-zero exit code.
    ChildExited {
        /// Exit code as reported by `Child::wait()`.
        code: i32,
    },
    /// Child was killed by a signal (SIGSEGV, SIGKILL, OOM-killer, …).
    ChildSignaled {
        /// Signal number; resolved through `ExitStatus::signal()` on
        /// Unix, or `0` on platforms where signal info is unavailable.
        signal: i32,
    },
    /// `Child::try_wait()` returned an I/O error (rare; usually means
    /// the OS reaped the child out from under us).
    WaitFailed {
        /// `io::Error` description.
        message: String,
    },
    /// A blocking `request()` returned a broken-pipe / EOF I/O error
    /// before `try_wait()` had a chance to confirm the child was gone.
    BrokenPipe,
}

impl RestartReason {
    /// Stable string tag for the JSON-RPC status payload. Keep in
    /// lockstep with the doc comment on
    /// [`crate::mcp::tool_lsp_cpp_status_schema`] (if added)
    /// and the `last_restart_reason` field of the public schema.
    pub(crate) fn as_tag(&self) -> &'static str {
        match self {
            RestartReason::InitialSpawn => "initial_spawn",
            RestartReason::ChildExited { .. } => "child_exited",
            RestartReason::ChildSignaled { .. } => "child_signaled",
            RestartReason::WaitFailed { .. } => "wait_failed",
            RestartReason::BrokenPipe => "broken_pipe",
        }
    }
}

/// Decision the supervisor returns at each request boundary. The MCP
/// loop dispatches on this; the supervisor itself does no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// Child is believed healthy; proceed with the request.
    Proceed,
    /// Child is dead but we are inside the backoff window. Sleep
    /// `wait` before retrying.
    Wait {
        /// Duration to sleep before the next spawn attempt.
        wait: Duration,
    },
    /// Restart budget exhausted; surface
    /// `supervisor_max_retries` to the caller. The caller should NOT
    /// keep looping — return the error to the MCP client.
    Fail {
        /// How long until the failed-state window expires and the
        /// supervisor will accept retry attempts again. Operator-facing
        /// hint, not a hard guarantee.
        retry_after: Duration,
    },
}

/// Pure-data supervisor policy. The MCP loop owns the actual
/// `Child` handle; this struct owns only counters, timestamps, and
/// the state-machine label.
///
/// Time is parameterised on a `Now` callable so unit tests can drive
/// the state machine deterministically. Production code calls
/// [`SupervisorPolicy::with_system_clock`] which threads
/// `std::time::Instant::now`.
pub(crate) struct SupervisorPolicy {
    state: SupervisorState,
    /// Sliding window of restart timestamps (monotonic instants
    /// expressed as ns since some arbitrary epoch — we store the raw
    /// `u128` returned by the clock so `MockClock` and `SystemClock`
    /// can both feed it). Pruned to entries inside the last
    /// [`RESTART_WINDOW`] on every observe_*.
    restart_history_ns: Vec<u128>,
    /// Monotonic time of the most recent spawn (`None` until the
    /// first successful spawn). Used by `record_exit` to evaluate the
    /// `HEALTHY_UPTIME_RESET` threshold; distinct from
    /// `state_entered_ns` because the backoff-reset rule cares
    /// specifically about *spawn-to-crash* uptime, not "time since
    /// any state transition."
    last_spawn_ns: Option<u128>,
    /// Monotonic time the supervisor entered its current state.
    /// `None` only in the pristine `Stopped` state before any spawn
    /// has fired. Updated by every state transition:
    ///
    /// - `record_spawn` -> `Running` at spawn time.
    /// - `record_exit` -> `Backoff` or `Failed` at crash time.
    /// - `should_retry` (Failed -> Stopped reset path) at window-
    ///   expiry observation time.
    ///
    /// Backs `current_uptime`, whose semantics is "wall-clock elapsed
    /// since the most recent state-relevant event." See that method's
    /// doc comment for per-state anchor definitions.
    state_entered_ns: Option<u128>,
    /// Most recent restart reason, surfaced through the status RPC.
    last_reason: Option<RestartReason>,
    /// Total restart count for observability. Distinct from
    /// `restart_history_ns.len()` (which only retains in-window
    /// entries) so the status RPC can report the lifetime count.
    total_restarts: u64,
    /// Current backoff duration. Doubled on each crash, reset to
    /// [`INITIAL_BACKOFF`] after a [`HEALTHY_UPTIME_RESET`] uptime
    /// window.
    current_backoff: Duration,
    /// Clock callback. Returns "ns since some monotonic epoch" — the
    /// absolute origin does not matter, only differences do.
    now_ns: Box<dyn Fn() -> u128 + Send + Sync>,
    /// Cumulative wall-clock cost (ns) of the supervisor's
    /// `Child::try_wait`-based liveness probe across the lifetime of
    /// this policy. Per Standing Rule 14 — every dispatched MCP request
    /// crosses [`crate::clangd::Clangd::is_alive`] (twice, in fact:
    /// once in the unconditional status-tool fast path, once in the
    /// pre-dispatch liveness gate), so the per-request constant cost
    /// shows up on every long-lived session. The accumulator is a
    /// monotonic `u64` ns counter wrapped by
    /// [`SupervisorPolicy::record_try_wait_ns`]; the `lsp_cpp_status`
    /// RPC reads it via [`SupervisorPolicy::try_wait_total_ns`] so an
    /// operator can divide by `request_count` (post-merge follow-up)
    /// to get amortised probe cost.
    try_wait_total_ns: u64,
}

impl std::fmt::Debug for SupervisorPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorPolicy")
            .field("state", &self.state)
            .field("restart_history_ns", &self.restart_history_ns)
            .field("last_spawn_ns", &self.last_spawn_ns)
            .field("state_entered_ns", &self.state_entered_ns)
            .field("last_reason", &self.last_reason)
            .field("total_restarts", &self.total_restarts)
            .field("current_backoff", &self.current_backoff)
            .field("try_wait_total_ns", &self.try_wait_total_ns)
            .finish_non_exhaustive()
    }
}

impl SupervisorPolicy {
    /// Build a policy that reads the system monotonic clock. Used by
    /// production MCP wiring. Tests use [`SupervisorPolicy::with_clock`]
    /// to inject a `MockClock`.
    pub(crate) fn with_system_clock() -> Self {
        use std::time::Instant;
        let origin = Instant::now();
        Self::with_clock(Box::new(move || {
            Instant::now().saturating_duration_since(origin).as_nanos()
        }))
    }

    /// Build a policy with an arbitrary clock source. Useful for
    /// deterministic unit tests.
    pub(crate) fn with_clock(now_ns: Box<dyn Fn() -> u128 + Send + Sync>) -> Self {
        Self {
            state: SupervisorState::Stopped,
            restart_history_ns: Vec::with_capacity(MAX_RESTARTS_IN_WINDOW * 2),
            last_spawn_ns: None,
            state_entered_ns: None,
            last_reason: None,
            total_restarts: 0,
            // Pre-INITIAL value; the first `record_exit` lifts it to
            // INITIAL_BACKOFF, the second doubles to 2 s, and so on
            // (1 -> 2 -> 4 -> 8 -> 16 -> 16 cap). Without this sentinel
            // the first crash would already report 2 s because we
            // double on the first observation.
            current_backoff: Duration::ZERO,
            now_ns,
            try_wait_total_ns: 0,
        }
    }

    /// Snapshot of the current logical state.
    pub(crate) fn current_state(&self) -> &SupervisorState {
        &self.state
    }

    /// Lifetime restart counter (does NOT decay with the sliding
    /// window). Surfaced through the status RPC.
    pub(crate) fn total_restarts(&self) -> u64 {
        self.total_restarts
    }

    /// Most recent restart reason, if any.
    pub(crate) fn last_reason(&self) -> Option<&RestartReason> {
        self.last_reason.as_ref()
    }

    /// Cumulative wall-clock cost of the `Child::try_wait`-backed
    /// liveness probe (ns). Standing Rule 14 perf window for the
    /// supervisor's poll cadence: every MCP request crosses
    /// [`crate::clangd::Clangd::is_alive`] twice (status fast path +
    /// pre-dispatch gate), so this counter scales linearly with
    /// session length. Surfaced through the `lsp_cpp_status`
    /// RPC; intended for offline regression checks (compare against a
    /// per-merge baseline) rather than runtime branching.
    pub(crate) fn try_wait_total_ns(&self) -> u64 {
        self.try_wait_total_ns
    }

    /// Add `ns` to the cumulative liveness-probe cost counter. Caller
    /// (the MCP loop in `mcp.rs`) wraps each `backend.is_alive()` call
    /// in `std::time::Instant::now()` / `elapsed()` and forwards the
    /// nanosecond delta here. Saturating-add so a runaway clock can
    /// never wrap silently to a small value (would mask a regression).
    pub(crate) fn record_try_wait_ns(&mut self, ns: u64) {
        self.try_wait_total_ns = self.try_wait_total_ns.saturating_add(ns);
    }

    /// Wall-clock elapsed since the most recent state-relevant event.
    /// The "anchor" event differs by state, but the returned
    /// `Duration` is meaningful in all cases — never silently zero
    /// outside `Running`. (Pre-LOW-AR3 the method returned
    /// `Duration::ZERO` for every state except `Running`, masking the
    /// "this just crashed" diagnostic an operator wants from the
    /// `lsp_cpp_status` payload.)
    ///
    /// Per-state anchor:
    ///
    /// | state         | anchor                                      |
    /// |---------------|---------------------------------------------|
    /// | `Running`     | most recent successful spawn                |
    /// | `Backoff{..}` | crash that triggered the Backoff transition |
    /// | `Failed`      | crash that tripped the restart-budget cap   |
    /// | `Stopped`     | most recent transition into `Stopped`       |
    /// |               | (Failed-window-expired reset path)          |
    /// | `Stopped`     | `Duration::ZERO` — pristine, no spawn yet   |
    /// |  (initial)    |                                             |
    ///
    /// Callers comparing the value across state transitions should
    /// also read `current_state()` to interpret the anchor; the
    /// `lsp_cpp_status` payload exposes both fields side-by-side
    /// for that reason.
    pub(crate) fn current_uptime(&self) -> Duration {
        match self.state_entered_ns {
            Some(anchor_ns) => {
                let now = (self.now_ns)();
                Duration::from_nanos(now.saturating_sub(anchor_ns) as u64)
            }
            None => Duration::ZERO,
        }
    }

    /// Record a successful spawn. The MCP loop calls this immediately
    /// after a `Clangd::spawn()` returns `Ok(())`.
    ///
    /// First-spawn special case: if `state` is `Stopped` and there has
    /// been no prior spawn, this is the initial bring-up — backoff is
    /// untouched, no restart history entry is added.
    pub(crate) fn record_spawn(&mut self) {
        let now = (self.now_ns)();
        let is_initial = self.last_spawn_ns.is_none();
        self.last_spawn_ns = Some(now);
        self.state_entered_ns = Some(now);
        self.state = SupervisorState::Running;
        if is_initial {
            self.last_reason = Some(RestartReason::InitialSpawn);
        }
    }

    /// Record an observed child exit. The MCP loop calls this after
    /// `Child::try_wait()` returns `Ok(Some(_))` or after a request
    /// errors with broken-pipe.
    ///
    /// Effects:
    ///
    /// - Bump `total_restarts`.
    /// - Append `now` to `restart_history_ns`, prune entries older
    ///   than [`RESTART_WINDOW`].
    /// - If `current_uptime() >= HEALTHY_UPTIME_RESET`, reset
    ///   `current_backoff` to [`INITIAL_BACKOFF`].
    /// - Otherwise double `current_backoff` up to [`MAX_BACKOFF`].
    /// - If the in-window restart count is now >= [`MAX_RESTARTS_IN_WINDOW`],
    ///   transition to `Failed`. Otherwise transition to
    ///   `Backoff { duration: current_backoff }`.
    pub(crate) fn record_exit(&mut self, reason: RestartReason) {
        let now = (self.now_ns)();
        let uptime = match self.last_spawn_ns {
            Some(spawn_ns) => Duration::from_nanos(now.saturating_sub(spawn_ns) as u64),
            None => Duration::ZERO,
        };

        // Decide backoff BEFORE we mutate state — the reset condition
        // depends on the previous backoff value.
        //
        // Sentinel: `current_backoff == ZERO` means "first crash since
        // reset". Lift to `INITIAL_BACKOFF` (1 s) — subsequent crashes
        // double. This produces the canonical 1 -> 2 -> 4 -> 8 -> 16
        // -> 16 cap curve.
        let next_backoff = if uptime >= HEALTHY_UPTIME_RESET {
            INITIAL_BACKOFF
        } else if self.current_backoff < INITIAL_BACKOFF {
            INITIAL_BACKOFF
        } else {
            let doubled = self.current_backoff.saturating_mul(2);
            if doubled > MAX_BACKOFF {
                MAX_BACKOFF
            } else {
                doubled
            }
        };

        self.total_restarts += 1;
        self.last_reason = Some(reason);
        self.restart_history_ns.push(now);
        self.prune_history(now);
        self.current_backoff = next_backoff;

        // Anchor `current_uptime` at the crash itself for both
        // post-exit states. `now` here is the same `now_ns()` reading
        // used to push `restart_history_ns`, so the anchor is exactly
        // the crash timestamp the operator sees in the status RPC.
        self.state_entered_ns = Some(now);

        if self.restart_history_ns.len() >= MAX_RESTARTS_IN_WINDOW {
            self.state = SupervisorState::Failed;
        } else {
            self.state = SupervisorState::Backoff {
                duration: next_backoff,
            };
        }
    }

    /// Inspect-and-decide entry point the MCP loop calls *before*
    /// dispatching a request. Pure observation — does NOT mutate
    /// state. The caller mutates by calling `record_spawn` /
    /// `record_exit` after acting.
    ///
    /// Decision matrix:
    ///
    /// | state         | returns                              |
    /// |---------------|--------------------------------------|
    /// | `Running`     | `Proceed`                            |
    /// | `Stopped`     | `Proceed` (caller must spawn first)  |
    /// | `Backoff{d}`  | `Wait { wait: d }`                   |
    /// | `Failed`      | `Fail { retry_after }` until window  |
    /// |               | slides; then transitions to          |
    /// |               | `Stopped` and returns `Proceed`      |
    pub(crate) fn should_retry(&mut self) -> RetryDecision {
        let now = (self.now_ns)();
        // Failed state has an implicit "expire after RESTART_WINDOW"
        // reset — checked here because we're idempotent on the
        // observation, even though we mutate the `Failed`-to-`Stopped`
        // transition.
        if matches!(self.state, SupervisorState::Failed) {
            self.prune_history(now);
            if self.restart_history_ns.len() < MAX_RESTARTS_IN_WINDOW {
                self.state = SupervisorState::Stopped;
                // Anchor `current_uptime` at the moment of reset so
                // operators see "time since the failed-window expired"
                // rather than "time since the original tripping crash."
                // The latter would keep growing indefinitely after the
                // window slides, which is misleading for an idle
                // post-reset Stopped state.
                self.state_entered_ns = Some(now);
                // Pre-INITIAL sentinel — the next crash will lift
                // back to INITIAL_BACKOFF (1 s), matching the
                // first-spawn-after-reset shape.
                self.current_backoff = Duration::ZERO;
            } else {
                let oldest = self.restart_history_ns.first().copied().unwrap_or(now);
                let elapsed = Duration::from_nanos(now.saturating_sub(oldest) as u64);
                let retry_after = RESTART_WINDOW.saturating_sub(elapsed);
                return RetryDecision::Fail { retry_after };
            }
        }

        // Backoff state: snapshot the duration before any mutation so the
        // borrow on `self.state` is dropped before the `Stopped` reassignment
        // below. Pre-patch this arm always returned `Wait { wait: *duration }`
        // regardless of how long had elapsed since `record_exit` set
        // `state_entered_ns`, so once a clangd crash drove the supervisor into
        // Backoff every subsequent MCP request looped forever returning
        // `Wait` — the only recovery was a full CC session restart. The
        // expiry check below restores the documented state-machine edge
        // `Backoff(d) -- d elapsed --> Stopped` (see the module-doc ASCII
        // diagram around line 51) and matches the same idempotent
        // observation pattern already used by the `Failed -> Stopped` branch.
        if let SupervisorState::Backoff { duration } = self.state {
            let entered_at = self.state_entered_ns.unwrap_or(now);
            let elapsed = Duration::from_nanos(now.saturating_sub(entered_at) as u64);
            if elapsed >= duration {
                self.state = SupervisorState::Stopped;
                self.state_entered_ns = Some(now);
                return RetryDecision::Proceed;
            } else {
                return RetryDecision::Wait {
                    wait: duration - elapsed,
                };
            }
        }

        match &self.state {
            SupervisorState::Running | SupervisorState::Stopped => RetryDecision::Proceed,
            SupervisorState::Backoff { .. } => unreachable!("handled above"),
            SupervisorState::Failed => unreachable!("handled above"),
        }
    }

    /// Drop history entries older than [`RESTART_WINDOW`].
    fn prune_history(&mut self, now: u128) {
        let cutoff_ns = RESTART_WINDOW.as_nanos();
        self.restart_history_ns
            .retain(|&ts| now.saturating_sub(ts) <= cutoff_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mutable mock clock — tests advance time in controlled steps.
    fn mock_clock() -> (Arc<Mutex<u128>>, Box<dyn Fn() -> u128 + Send + Sync>) {
        let cell = Arc::new(Mutex::new(0u128));
        let cell2 = Arc::clone(&cell);
        let f: Box<dyn Fn() -> u128 + Send + Sync> = Box::new(move || *cell2.lock().unwrap());
        (cell, f)
    }

    fn advance(clock: &Arc<Mutex<u128>>, by: Duration) {
        let mut guard = clock.lock().unwrap();
        *guard += by.as_nanos();
    }

    /// Backoff doubles 1 -> 2 -> 4 -> 8 -> 16 then caps. Counterfactual:
    /// removing the `MAX_BACKOFF` cap in `record_exit` would push the
    /// fifth crash to 32 s and fail the assertion.
    #[test]
    fn backoff_curve_doubles_then_caps_at_16s() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Initial spawn. No history entry, no backoff change.
        sup.record_spawn();
        assert_eq!(*sup.current_state(), SupervisorState::Running);

        // Crash immediately (uptime < HEALTHY_UPTIME_RESET) — first
        // backoff is INITIAL_BACKOFF (1 s).
        sup.record_exit(RestartReason::ChildSignaled { signal: 9 });
        assert_eq!(
            *sup.current_state(),
            SupervisorState::Backoff {
                duration: Duration::from_secs(1)
            },
            "first crash should yield 1 s backoff"
        );

        // Each subsequent crash doubles. The respawn / re-crash cycle
        // happens fast enough that the healthy-uptime reset does NOT
        // fire (we advance time by 0 between record_spawn and
        // record_exit). Expected: 2, 4, 8, 16, 16.
        let expected = [2, 4, 8, 16, 16];
        for (i, secs) in expected.iter().enumerate() {
            // Stay tightly inside the 5-min window.
            advance(&clock, Duration::from_millis(10));
            sup.record_spawn();
            advance(&clock, Duration::from_millis(10));
            sup.record_exit(RestartReason::BrokenPipe);
            // Once we hit MAX_RESTARTS_IN_WINDOW (5) the state pivots
            // to Failed and the Backoff duration test does not apply.
            // First four iterations land in Backoff; the 5th lands in
            // Failed because total restarts in window = 5
            // (1 from outside loop + 4 here = 5 -> Failed transition).
            if i < 3 {
                assert_eq!(
                    *sup.current_state(),
                    SupervisorState::Backoff {
                        duration: Duration::from_secs(*secs),
                    },
                    "iteration {i}: expected {secs} s backoff, got {:?}",
                    sup.current_state()
                );
            } else {
                // 4th and 5th iterations: history has 5+ entries -> Failed.
                assert!(
                    matches!(sup.current_state(), SupervisorState::Failed),
                    "iteration {i}: expected Failed state, got {:?}",
                    sup.current_state()
                );
                break;
            }
        }
    }

    /// Five restarts inside the 5 min window trip Failed; the next
    /// `should_retry()` returns `Fail` with a positive `retry_after`.
    /// After the window expires the supervisor reverts to `Stopped`
    /// and returns `Proceed`. Counterfactual: deleting the
    /// `restart_history_ns.len() >= MAX_RESTARTS_IN_WINDOW` branch in
    /// `record_exit` would let the 5th crash land in `Backoff` and
    /// fail the `Failed`-state assertion.
    #[test]
    fn five_crashes_in_window_trip_failed_then_window_expires() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        for _ in 0..5 {
            advance(&clock, Duration::from_millis(100));
            sup.record_spawn();
            advance(&clock, Duration::from_millis(100));
            sup.record_exit(RestartReason::BrokenPipe);
        }

        assert!(
            matches!(sup.current_state(), SupervisorState::Failed),
            "5 crashes inside RESTART_WINDOW must reach Failed; got {:?}",
            sup.current_state()
        );
        assert_eq!(sup.total_restarts(), 5);

        // Failed-state should_retry() returns Fail with positive retry_after.
        match sup.should_retry() {
            RetryDecision::Fail { retry_after } => {
                assert!(retry_after > Duration::ZERO, "retry_after must be > 0");
                assert!(
                    retry_after <= RESTART_WINDOW,
                    "retry_after {retry_after:?} must not exceed RESTART_WINDOW"
                );
            }
            other => panic!("expected Fail in Failed state, got {other:?}"),
        }

        // Advance past the window. Pruning + state reset on next
        // observation.
        advance(&clock, RESTART_WINDOW + Duration::from_secs(1));
        match sup.should_retry() {
            RetryDecision::Proceed => {} // good
            other => panic!("expected Proceed after window expired, got {other:?}"),
        }
        assert_eq!(*sup.current_state(), SupervisorState::Stopped);
    }

    /// Healthy uptime (>= 60 s) resets the backoff curve so a single
    /// transient crash after a long uptime starts at 1 s, not 16 s.
    /// Counterfactual: removing the `if uptime >= HEALTHY_UPTIME_RESET`
    /// branch in `record_exit` makes this assertion fail (post-reset
    /// crash would inherit MAX_BACKOFF from the prior crash burst).
    #[test]
    fn healthy_uptime_resets_backoff() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Burn the backoff up to MAX via 4 fast crashes.
        for _ in 0..4 {
            advance(&clock, Duration::from_millis(10));
            sup.record_spawn();
            advance(&clock, Duration::from_millis(10));
            sup.record_exit(RestartReason::ChildExited { code: 1 });
        }
        // Should be at 16 s backoff (8 doubled, capped) but still
        // below the 5-restart threshold (4 entries).
        assert_eq!(
            *sup.current_state(),
            SupervisorState::Backoff {
                duration: Duration::from_secs(8),
            },
            "after 4 crashes the backoff should be 8 s (1, 2, 4, 8); got {:?}",
            sup.current_state()
        );

        // Now spawn and run for >60 s before crashing again. The
        // backoff should reset to 1 s (the entry threshold).
        advance(&clock, Duration::from_millis(100));
        sup.record_spawn();
        advance(&clock, HEALTHY_UPTIME_RESET + Duration::from_secs(1));
        sup.record_exit(RestartReason::BrokenPipe);
        // 5th crash inside RESTART_WINDOW -> Failed, but the backoff
        // value was reset prior to the state transition. We can't
        // observe the reset directly through `current_state()` because
        // Failed shadows Backoff; observe via internal field.
        assert_eq!(
            sup.current_backoff, INITIAL_BACKOFF,
            "healthy uptime should have reset backoff curve"
        );
    }

    /// `should_retry()` behaviour by state. Counterfactual: collapsing
    /// the `Backoff` branch into `Proceed` would make the second
    /// assertion fail.
    #[test]
    fn should_retry_decision_matrix() {
        let (_clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Stopped -> Proceed (caller spawns).
        assert_eq!(sup.should_retry(), RetryDecision::Proceed);

        sup.record_spawn();
        // Running -> Proceed.
        assert_eq!(sup.should_retry(), RetryDecision::Proceed);

        sup.record_exit(RestartReason::BrokenPipe);
        // Backoff -> Wait.
        match sup.should_retry() {
            RetryDecision::Wait { wait } => {
                assert_eq!(wait, Duration::from_secs(1));
            }
            other => panic!("expected Wait in Backoff state, got {other:?}"),
        }
    }

    /// LOW-AR3 anchor — `current_uptime` returns wall-clock elapsed in
    /// every state, not just `Running`. Each sub-assertion below
    /// targets a separate state-anchor branch in `current_uptime`.
    /// Counterfactual: reverting the implementation to the
    /// Running-only form (`match (&self.state, self.last_spawn_ns) {
    /// (Running, Some(spawn_ns)) => ..., _ => ZERO }`) makes every
    /// non-Running assertion below fail, which is exactly the
    /// regression LOW-AR3 was filed to prevent.
    #[test]
    fn current_uptime_spans_all_states() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Initial Stopped (no spawn ever) -> ZERO. The pristine
        // `state_entered_ns = None` branch.
        assert_eq!(*sup.current_state(), SupervisorState::Stopped);
        assert_eq!(
            sup.current_uptime(),
            Duration::ZERO,
            "pristine Stopped (no spawn yet) must report zero uptime"
        );

        // Running -> increments with wall time. Anchor is the spawn.
        sup.record_spawn();
        advance(&clock, Duration::from_secs(7));
        assert_eq!(*sup.current_state(), SupervisorState::Running);
        assert_eq!(
            sup.current_uptime(),
            Duration::from_secs(7),
            "Running uptime must be wall time since spawn"
        );

        // Crash. State transitions to Backoff; the anchor is the
        // crash timestamp itself, so `current_uptime` == 0 right at
        // the transition, then increments as wall time passes.
        sup.record_exit(RestartReason::BrokenPipe);
        assert!(
            matches!(sup.current_state(), SupervisorState::Backoff { .. }),
            "expected Backoff after first crash, got {:?}",
            sup.current_state()
        );
        assert_eq!(
            sup.current_uptime(),
            Duration::ZERO,
            "uptime is zero at the instant of state transition into Backoff"
        );
        advance(&clock, Duration::from_secs(3));
        assert_eq!(
            sup.current_uptime(),
            Duration::from_secs(3),
            "Backoff uptime must reflect time since the triggering crash, \
             not zero (LOW-AR3 regression target)"
        );

        // Drive to Failed by tripping the restart-budget cap. Anchor
        // remains the latest crash; `current_uptime` keeps reporting
        // time since that crash through the Failed window.
        for _ in 0..4 {
            advance(&clock, Duration::from_millis(10));
            sup.record_spawn();
            advance(&clock, Duration::from_millis(10));
            sup.record_exit(RestartReason::BrokenPipe);
        }
        assert!(
            matches!(sup.current_state(), SupervisorState::Failed),
            "expected Failed after 5 in-window crashes, got {:?}",
            sup.current_state()
        );
        let pre_advance_uptime = sup.current_uptime();
        advance(&clock, Duration::from_secs(2));
        let post_advance_uptime = sup.current_uptime();
        assert_eq!(
            post_advance_uptime - pre_advance_uptime,
            Duration::from_secs(2),
            "Failed-state uptime must continue tracking wall time \
             from the tripping crash"
        );

        // Slide past the restart window. `should_retry()` resets the
        // supervisor to Stopped and re-anchors `state_entered_ns` at
        // the reset moment so post-reset uptime starts at zero.
        advance(&clock, RESTART_WINDOW + Duration::from_secs(1));
        let _ = sup.should_retry();
        assert_eq!(*sup.current_state(), SupervisorState::Stopped);
        assert_eq!(
            sup.current_uptime(),
            Duration::ZERO,
            "post-Failed Stopped reset must re-anchor uptime at the reset \
             moment, not keep growing from the original crash"
        );
        advance(&clock, Duration::from_secs(4));
        assert_eq!(
            sup.current_uptime(),
            Duration::from_secs(4),
            "post-reset Stopped uptime tracks time since the reset"
        );
    }

    /// Mutation probe: `current_uptime` must consult
    /// `state_entered_ns`, not just `last_spawn_ns`. If the impl
    /// reverts to keying off `last_spawn_ns` (the pre-LOW-AR3 shape)
    /// this test catches it because the spawn anchor would still be
    /// "long ago" while the Backoff anchor is "just now."
    #[test]
    fn current_uptime_uses_state_anchor_not_spawn_anchor() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Long-running successful spawn.
        sup.record_spawn();
        advance(&clock, Duration::from_secs(120));

        // Crash. Anchor pivots from spawn (120 s ago) to crash (0 s).
        sup.record_exit(RestartReason::BrokenPipe);
        assert_eq!(
            sup.current_uptime(),
            Duration::ZERO,
            "post-crash anchor must be the crash, not the long-ago spawn"
        );

        advance(&clock, Duration::from_secs(5));
        // If `current_uptime` were keyed off `last_spawn_ns`, this
        // would report 125 s. With the LOW-AR3 fix it reports 5 s.
        assert_eq!(
            sup.current_uptime(),
            Duration::from_secs(5),
            "Backoff uptime must measure since-crash, not since-spawn"
        );
        assert_ne!(
            sup.current_uptime(),
            Duration::from_secs(125),
            "if this assertion fires, current_uptime regressed to \
             keying off last_spawn_ns (the pre-LOW-AR3 shape)"
        );
    }

    /// Standing-Rule-14 perf-window positive case: N=10 invocations of
    /// `record_try_wait_ns` against a fresh policy land monotonically
    /// increasing, non-zero values in `try_wait_total_ns`. Counterfactual:
    /// dropping the `saturating_add` body in `record_try_wait_ns` (e.g.
    /// `self.try_wait_total_ns = ns`) breaks monotonicity on the second
    /// iteration and fails this test.
    #[test]
    fn try_wait_total_ns_accumulates_monotonically() {
        let (_clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Sanity: fresh policy starts at zero.
        assert_eq!(
            sup.try_wait_total_ns(),
            0,
            "fresh policy must report zero probe cost"
        );

        let mut prev = 0u64;
        for i in 1..=10u64 {
            sup.record_try_wait_ns(100);
            let now_total = sup.try_wait_total_ns();
            assert_eq!(
                now_total,
                100 * i,
                "iteration {i}: expected exact accumulation 100*{i} = {}, got {now_total}",
                100 * i
            );
            assert!(
                now_total > prev,
                "iteration {i}: accumulator must be strictly monotonic; prev {prev}, now {now_total}",
            );
            prev = now_total;
        }
    }

    /// Standing-Rule-14 perf-window negative case: zero invocations of
    /// `record_try_wait_ns` keep the counter at 0. Counterfactual:
    /// initialising `try_wait_total_ns` to a non-zero sentinel in
    /// `with_clock` would fail this assertion. Pairs with the positive
    /// case above to nail down the "zero on construction, grows on
    /// record" contract.
    #[test]
    fn try_wait_total_ns_zero_without_invocations() {
        let (_clock, now) = mock_clock();
        let sup = SupervisorPolicy::with_clock(now);
        assert_eq!(
            sup.try_wait_total_ns(),
            0,
            "no record_try_wait_ns calls => counter stays at 0"
        );
    }

    /// Mutation-probe sentinel for the call-site contract in `mcp.rs`:
    /// the wrapper around `backend.is_alive()` MUST capture the
    /// `Instant::now()` delta and feed it through
    /// `record_try_wait_ns`. Drop the timing wrapper (i.e. the call
    /// site never invokes `record_try_wait_ns`) and the accumulator
    /// stays at 0 even after N "probes." This test simulates the
    /// dropped-wrapper failure mode by NOT calling
    /// `record_try_wait_ns`; the accumulator must remain at 0,
    /// proving the positive test above genuinely depends on the
    /// `Instant::now()` wrap rather than passing trivially.
    #[test]
    fn try_wait_total_ns_stays_zero_when_wrapper_missing() {
        let (_clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Simulate 10 probe call sites that "forgot" to invoke the
        // recorder. State-machine traffic still flows (record_spawn,
        // record_exit) but the perf-window hook is absent.
        for _ in 0..10 {
            sup.record_spawn();
            sup.record_exit(RestartReason::BrokenPipe);
        }

        assert_eq!(
            sup.try_wait_total_ns(),
            0,
            "no record_try_wait_ns calls => counter stays at 0 even with state churn",
        );
    }

    /// Regression: pre-patch `should_retry()` only handled `Failed` expiry.
    /// `Backoff` returned `Wait { wait: duration }` indefinitely because no
    /// branch consulted `state_entered_ns`. Once a clangd crash drove the
    /// supervisor into Backoff, every subsequent MCP request received `Wait`
    /// forever — recovery required a full CC session restart. This test
    /// pins the new Backoff-expiry edge: after the duration elapses,
    /// `should_retry()` transitions Backoff -> Stopped and returns Proceed.
    /// Counterfactual: reverting the new `if let SupervisorState::Backoff`
    /// branch in `should_retry()` makes the post-elapse assertion fall into
    /// the unchanged `Wait` arm and this test fails.
    #[test]
    fn backoff_expires_and_returns_proceed_after_window() {
        let (clock, now) = mock_clock();
        let mut sup = SupervisorPolicy::with_clock(now);

        // Enter Backoff via the standard spawn -> exit path. First crash
        // yields a 1 s backoff (INITIAL_BACKOFF).
        sup.record_spawn();
        sup.record_exit(RestartReason::BrokenPipe);
        let entry_state = sup.current_state().clone();
        assert!(
            matches!(entry_state, SupervisorState::Backoff { .. }),
            "expected Backoff after first crash, got {entry_state:?}",
        );
        let backoff_duration = match entry_state {
            SupervisorState::Backoff { duration } => duration,
            _ => unreachable!(),
        };
        assert_eq!(backoff_duration, Duration::from_secs(1));

        // Pre-elapse: should_retry must still report Wait, but with a
        // shrinking remainder rather than the static full `duration`.
        // This sub-assertion guards against the buggy "always full
        // duration" pre-patch shape — even when there's still time left,
        // the wait must be relative to `state_entered_ns`.
        advance(&clock, Duration::from_millis(250));
        match sup.should_retry() {
            RetryDecision::Wait { wait } => {
                assert!(
                    wait < backoff_duration,
                    "pre-elapse wait must shrink; backoff={backoff_duration:?} wait={wait:?}",
                );
                assert!(wait > Duration::ZERO, "pre-elapse wait must be > 0");
            }
            other => panic!("expected Wait inside backoff window, got {other:?}"),
        }
        // Confirm the observation did NOT mutate state — should_retry is
        // documented as a no-op observer in the pre-elapse case.
        assert!(
            matches!(sup.current_state(), SupervisorState::Backoff { .. }),
            "should_retry inside backoff window must not mutate state"
        );

        // Advance past the backoff duration. Next observation must
        // transition Backoff -> Stopped and return Proceed.
        advance(&clock, backoff_duration + Duration::from_millis(10));
        match sup.should_retry() {
            RetryDecision::Proceed => {}
            other => panic!("expected Proceed after backoff expired, got {other:?}"),
        }
        assert_eq!(
            *sup.current_state(),
            SupervisorState::Stopped,
            "expired Backoff must transition to Stopped",
        );
        // The reset must re-anchor `state_entered_ns` so post-reset
        // current_uptime starts at zero (mirrors the Failed-window-expired
        // reset path).
        assert_eq!(
            sup.current_uptime(),
            Duration::ZERO,
            "post-reset Stopped must re-anchor uptime at the reset moment",
        );
    }
}
