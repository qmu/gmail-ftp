//! [`Scheduler`] — the pure dispatch orchestration (RFD §6/§8). `tick()` is one evaluation pass
//! (the daemon calls it in a loop; the CF Cron Trigger calls it once per fire); `dispatch()`
//! fires one due boundary idempotently. The scheduler constructs no effects and performs no I/O —
//! it drives the [`JobStore`], [`Clock`], and [`Committer`] seams only (the purity invariant).

use crate::clock::Clock;
use crate::commit::{CommitError, Committer};
use crate::hash::sha256_hex;
use crate::lastrun::bind_last_run;
use crate::schedule::Instant;
use crate::store::{JobBinding, JobStore, RunRecord, RunStatus};

/// The default single-flight lease TTL (seconds): long enough to cover a slow commit, short
/// enough to self-heal a crashed holder.
const DEFAULT_LEASE_TTL: Instant = 300;

/// The circuit-breaker threshold: after this many consecutive failures a JOB is auto-disabled
/// with a ledger note (RFD §6 observability).
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// The sentinel `LAST_RUN()` value on a JOB's first run (`last_run_at = None`): epoch 0.
const FIRST_RUN_SENTINEL: Instant = 0;

/// What a `dispatch` produced, for the `tick` summary (secret-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// The JOB name.
    pub job: String,
    /// The deterministic run-id.
    pub run_id: String,
    /// The boundary dispatched.
    pub scheduled_for: Instant,
    /// The status of the dispatch.
    pub status: RunStatus,
    /// Whether this dispatch actually committed (false = no-op: lease lost or already committed).
    pub committed: bool,
}

/// The JOB scheduler over an injected [`JobStore`], [`Clock`], and [`Committer`]. Pure: no tokio,
/// no global state, no I/O of its own — wasm-clean.
pub struct Scheduler<S, C, M> {
    store: S,
    clock: C,
    committer: M,
    lease_ttl: Instant,
}

impl<S, C, M> Scheduler<S, C, M>
where
    S: JobStore,
    C: Clock,
    M: Committer,
{
    /// Construct a scheduler over the three seams (default lease TTL).
    pub fn new(store: S, clock: C, committer: M) -> Self {
        Self {
            store,
            clock,
            committer,
            lease_ttl: DEFAULT_LEASE_TTL,
        }
    }

    /// Override the single-flight lease TTL (seconds).
    #[must_use]
    pub fn with_lease_ttl(mut self, ttl: Instant) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Borrow the store (for the daemon / tests to read the ledger).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The scheduler's current instant (the daemon uses it to seed deterministic jitter).
    #[must_use]
    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// One evaluation pass: load enabled JOBs → per JOB compute the due set from
    /// `run_state.last_run_at` + `now` folded by its [`MissedPolicy`] → dispatch each due
    /// boundary. Returns the per-dispatch summary (secret-free). A store error on one JOB is
    /// logged and skipped — it never tears down the whole pass.
    pub fn tick(&self) -> Vec<Dispatched> {
        let now = self.clock.now();
        let mut out = Vec::new();

        let jobs = match self.store.load_enabled() {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(target: "cfs::cron", reason = %e, "tick: cannot load enabled jobs");
                return out;
            }
        };

        for job in &jobs {
            let state = match self.store.run_state(&job.name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "cfs::cron", job = %job.name, reason = %e, "tick: cannot read run state");
                    continue;
                }
            };
            // Circuit-breaker: a disabled JOB is excluded by load_enabled, but a JOB that just
            // tripped the breaker mid-pass is recorded Disabled here too (belt-and-suspenders).
            if matches!(state.last_status, RunStatus::Disabled) {
                continue;
            }
            let due = job.missed.due_set(&job.schedule, state.last_run_at, now);
            for scheduled_for in due {
                out.push(self.dispatch(job, scheduled_for));
            }
        }
        out
    }

    /// Fire one due boundary for `job`, idempotently (RFD §6 exactly-once-ish):
    ///   1. derive the deterministic run-id = hash(job, scheduled_for);
    ///   2. if `(job, run_id)` already committed → no-op (retried run-id after success);
    ///   3. acquire the single-flight lease → if lost, no-op (a concurrent dispatch holds it);
    ///   4. rehydrate + rewrite the DO body (`LAST_RUN()` → stored boundary / sentinel);
    ///   5. commit through the injected committer under the JOB's policy;
    ///   6. on success: `record_run` advances `last_run_at` to **`scheduled_for` (NOT now)** and
    ///      stores `last_plan_hash`; on failure: leave `last_run_at` unmoved (re-cover next tick),
    ///      bump the failure counter, and auto-disable past the breaker threshold;
    ///   7. release the lease.
    pub fn dispatch(&self, job: &JobBinding, scheduled_for: Instant) -> Dispatched {
        let run_id = run_id_for(&job.name, scheduled_for);

        // (2) idempotency: a retried dispatch with the same run-id after success is a no-op.
        match self.store.is_committed(&job.name, &run_id) {
            Ok(true) => {
                return Dispatched {
                    job: job.name.clone(),
                    run_id,
                    scheduled_for,
                    status: RunStatus::Success,
                    committed: false,
                };
            }
            Ok(false) => {}
            Err(e) => {
                return self.dispatch_failed(job, &run_id, scheduled_for, e.to_string(), false)
            }
        }

        // (3) single-flight lease: exactly one of two concurrent dispatches commits.
        let lease = match self.store.acquire_lease(&job.name, &run_id, self.lease_ttl) {
            Ok(l) => l,
            Err(e) => {
                return self.dispatch_failed(job, &run_id, scheduled_for, e.to_string(), false)
            }
        };
        if !lease.acquired {
            // Another dispatch holds the lease — no-op (no record; the holder records).
            return Dispatched {
                job: job.name.clone(),
                run_id,
                scheduled_for,
                status: RunStatus::Success,
                committed: false,
            };
        }

        // (4) the boundary LAST_RUN() resolves to: the stored high-water mark, or the sentinel
        // epoch on first run. Read it fresh under the lease.
        let last_run_at = self
            .store
            .run_state(&job.name)
            .ok()
            .and_then(|s| s.last_run_at)
            .unwrap_or(FIRST_RUN_SENTINEL);

        let mut stmt = job.plan.statement().clone();
        bind_last_run(&mut stmt, last_run_at);

        // (5) commit through the injected committer under the JOB's policy (never widened).
        let result = self.committer.commit(&stmt, &job.policy);

        let dispatched = match result {
            Ok(outcome) => {
                // (6) success: advance last_run_at to scheduled_for, store the plan hash.
                // AT-LEAST-ONCE NOTE: the commit-apply (above) and this record_run are two steps,
                // not one atomic transaction. A crash BETWEEN them leaves the effect applied but
                // last_run_at un-advanced and the run-id un-dedup'd — so the next tick re-covers
                // the same window and re-dispatches the SAME deterministic run-id. This is safe,
                // not duplication: effects are idempotent (UPSERT/@version, RFD §6), so the
                // re-apply is a no-op-equivalent. The system is at-least-once by design; the
                // run-id dedup + idempotent effects make the worst case a safe replay.
                let record = RunRecord {
                    job: job.name.clone(),
                    run_id: run_id.clone(),
                    scheduled_for,
                    status: RunStatus::Success,
                    applied_count: outcome.affected,
                    plan_hash: Some(outcome.plan_hash),
                    failure_note: None,
                };
                let _ = self.store.record_run(record);
                tracing::info!(
                    target: "cfs::cron",
                    job = %job.name,
                    run_id = %run_id,
                    scheduled_for,
                    applied = outcome.affected,
                    "job fired successfully"
                );
                Dispatched {
                    job: job.name.clone(),
                    run_id: run_id.clone(),
                    scheduled_for,
                    status: RunStatus::Success,
                    committed: true,
                }
            }
            Err(err) => self.on_failure(job, &run_id, scheduled_for, &err),
        };

        // (7) release the lease (the TTL also expires it).
        self.store.release_lease(&job.name, &run_id);
        dispatched
    }

    /// Record a failed fire: leave `last_run_at` unmoved (the window re-covers next tick), bump
    /// the consecutive-failure counter, and auto-disable past the breaker threshold with a ledger
    /// note. The error reason is already secret-free (from [`CommitError`]).
    fn on_failure(
        &self,
        job: &JobBinding,
        run_id: &str,
        scheduled_for: Instant,
        err: &CommitError,
    ) -> Dispatched {
        let reason = err.to_string();
        let record = RunRecord {
            job: job.name.clone(),
            run_id: run_id.to_string(),
            scheduled_for,
            status: RunStatus::Failed,
            applied_count: 0,
            plan_hash: None,
            failure_note: Some(reason.clone()),
        };
        let _ = self.store.record_run(record);

        // Circuit-breaker: if this failure crosses the threshold, auto-disable with a ledger note.
        let failures = self
            .store
            .run_state(&job.name)
            .map(|s| s.consecutive_failures)
            .unwrap_or(0);
        if failures >= MAX_CONSECUTIVE_FAILURES {
            let note = RunRecord {
                job: job.name.clone(),
                run_id: run_id.to_string(),
                scheduled_for,
                status: RunStatus::Disabled,
                applied_count: 0,
                plan_hash: None,
                failure_note: Some(format!(
                    "auto-disabled after {failures} consecutive failures (circuit-breaker)"
                )),
            };
            let _ = self.store.record_run(note);
            tracing::warn!(
                target: "cfs::cron",
                job = %job.name,
                failures,
                "job auto-disabled by circuit-breaker"
            );
        }

        tracing::warn!(
            target: "cfs::cron",
            job = %job.name,
            run_id = %run_id,
            scheduled_for,
            reason = %reason,
            "job fire failed (window will re-cover next tick)"
        );
        Dispatched {
            job: job.name.clone(),
            run_id: run_id.to_string(),
            scheduled_for,
            status: RunStatus::Failed,
            committed: false,
        }
    }

    /// A pre-commit store failure (before we held the lease / built a plan): record nothing
    /// durable beyond the warning, report a failed dispatch.
    fn dispatch_failed(
        &self,
        job: &JobBinding,
        run_id: &str,
        scheduled_for: Instant,
        reason: String,
        committed: bool,
    ) -> Dispatched {
        tracing::warn!(
            target: "cfs::cron",
            job = %job.name,
            reason = %reason,
            "dispatch precondition failed"
        );
        Dispatched {
            job: job.name.clone(),
            run_id: run_id.to_string(),
            scheduled_for,
            status: RunStatus::Failed,
            committed,
        }
    }
}

/// The **deterministic** run-id for `(job, scheduled_for)`: `run-<16 lower-hex of
/// SHA-256("<job>\0<scheduled_for>")>`. Deterministic by design — a retried fire for the same
/// `scheduled_for` yields the SAME run-id, so the ledger dedups it (better than a random UUID, and
/// no uuid dep / no randomness in the pure path).
#[must_use]
pub fn run_id_for(job: &str, scheduled_for: Instant) -> String {
    let material = format!("{job}\u{0}{scheduled_for}");
    let hex = sha256_hex(material.as_bytes());
    format!("run-{}", &hex[..16])
}
