//! The **saga executor** and the single-source ACID path (RFD-0001 §6).
//!
//! `cfs-txn` is pure orchestration: it does not own async or I/O. The impure apply is
//! reached through the synchronous [`LegApplier`] seam — the runtime adapts its async
//! `ApplyDriver` (and a real driver's transaction handle) to it, while tests supply an
//! in-memory fake. The executors here implement the **policy**:
//!
//! - **Idempotency / resume.** Each leg runs `record_intent → (skip if already applied) →
//!   apply → mark_applied`. A re-run of the same plan re-applies nothing — every prior
//!   [`EffectKey`](crate::EffectKey) is `applied()`, so every leg is
//!   [`LegOutcome::AlreadyApplied`].
//! - **Optimistic concurrency.** The leg's [`Precondition`](crate::Precondition) is honored
//!   by the applier; a
//!   `Conflict` is bounded-auto-retried (re-read → re-apply) up to a cap, else surfaced typed.
//! - **Saga compensation.** On a hard failure, applied legs are compensated in **reverse**.
//! - **Irreversible.** An irreversible leg is never retried and never compensated (it cannot
//!   be undone) — its failure stops the saga; its success is final.

use cfs_plan::NodeId;

use crate::ledger::AuditLedger;
use crate::leg::{EffectLeg, LegApplier};
use crate::outcome::LegOutcome;
use crate::report::{LegRecord, RecoveryReport};
use crate::version::Version;

/// Tuning for the saga's optimistic-concurrency auto-retry (RFD §6). `conflict_retries` is
/// the number of re-read-then-write attempts after a `Conflict` before surfacing the typed
/// error; `0` means "surface the first conflict immediately".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SagaPolicy {
    /// Bounded re-read-then-write attempts on a `Conflict` (0 = no auto-retry).
    pub conflict_retries: u32,
}

impl Default for SagaPolicy {
    /// A conservative default: one bounded re-read on conflict, then surface typed.
    fn default() -> Self {
        Self {
            conflict_retries: 1,
        }
    }
}

/// Drives an ordered `Vec<EffectLeg>` to completion or recovery (RFD §6). Pure orchestration
/// over the [`LegApplier`] + [`AuditLedger`] seams; emits a [`RecoveryReport`].
pub struct SagaExecutor<'a> {
    ledger: &'a dyn AuditLedger,
    policy: SagaPolicy,
}

impl<'a> SagaExecutor<'a> {
    /// Construct an executor over a ledger with default policy.
    #[must_use]
    pub fn new(ledger: &'a dyn AuditLedger) -> Self {
        Self {
            ledger,
            policy: SagaPolicy::default(),
        }
    }

    /// Construct an executor with an explicit conflict-retry policy.
    #[must_use]
    pub fn with_policy(ledger: &'a dyn AuditLedger, policy: SagaPolicy) -> Self {
        Self { ledger, policy }
    }

    /// Apply one leg with the idempotency + optimistic-concurrency rules:
    /// 1. If the leg's key is already `applied()` → [`LegOutcome::AlreadyApplied`] (no apply).
    /// 2. Else `record_intent` (append-before-apply), then apply.
    /// 3. On `Conflict`, bounded re-read-then-write up to the policy cap (never for
    ///    irreversible legs — they are applied at most once).
    /// 4. On `Applied`, `mark_applied` (seal the ledger).
    fn apply_leg(&self, applier: &mut dyn LegApplier, leg: &EffectLeg) -> LegOutcome {
        // Idempotent resume / at-least-once dedup: a prior apply makes this a no-op.
        if self.ledger.applied(&leg.key).is_some() {
            return LegOutcome::AlreadyApplied;
        }
        // Append-before-apply: a crash after this leaves a reconstructable intent.
        self.ledger.record_intent(&leg.key, &leg.descriptor);

        let mut precondition = leg.descriptor.precondition.clone();
        // Attempt 0 + up to `conflict_retries` re-reads. Irreversible legs get a single
        // attempt (never re-applied), even on conflict.
        let max_conflict_attempts = if leg.descriptor.irreversible {
            0
        } else {
            self.policy.conflict_retries
        };

        let mut attempt = 0u32;
        loop {
            let outcome = applier.apply(leg, &precondition);
            match outcome {
                LegOutcome::Applied(receipt) => {
                    self.ledger.mark_applied(&leg.key, &receipt);
                    return LegOutcome::Applied(receipt);
                }
                LegOutcome::AlreadyApplied => return LegOutcome::AlreadyApplied,
                LegOutcome::Conflict(world_version) => {
                    if attempt >= max_conflict_attempts {
                        return LegOutcome::Conflict(world_version);
                    }
                    attempt += 1;
                    // Bounded re-read-then-write: re-base the precondition on the version the
                    // world actually holds and retry. The applier re-reads the row under the
                    // hood; here we advance the expected version so the next write is conditioned
                    // on fresh state (no lost update).
                    precondition = rebase_precondition(&world_version);
                }
                LegOutcome::Failed(e) => return LegOutcome::Failed(e),
            }
        }
    }

    /// Run the ordered legs as a saga (RFD §6): apply in order; on the first **hard** failure
    /// (`Failed` or an unrecovered `Conflict`), run the registered
    /// [`Compensation`](crate::Compensation) for every
    /// **applied** leg in reverse, then stop. Returns a [`RecoveryReport`]. `AlreadyApplied`
    /// legs count as success (idempotent resume) but are **not** compensated on a later
    /// failure (they were applied by a previous run — compensating them is that run's job).
    #[must_use]
    pub fn run_saga(&self, applier: &mut dyn LegApplier, legs: &[EffectLeg]) -> RecoveryReport {
        let mut records: Vec<LegRecord> = Vec::with_capacity(legs.len());
        // Legs applied **this run** (eligible for compensation), newest last.
        let mut applied_this_run: Vec<usize> = Vec::new();
        let mut failure_at: Option<NodeId> = None;

        for (i, leg) in legs.iter().enumerate() {
            if failure_at.is_some() {
                // After a hard failure the rest of the saga is not attempted.
                records.push(LegRecord::skipped(leg));
                continue;
            }
            let outcome = self.apply_leg(applier, leg);
            let is_fresh_apply = matches!(outcome, LegOutcome::Applied(_));
            let hard_failure = matches!(outcome, LegOutcome::Failed(_) | LegOutcome::Conflict(_));
            records.push(LegRecord::from_outcome(leg, outcome));
            if is_fresh_apply {
                applied_this_run.push(i);
            }
            if hard_failure {
                failure_at = Some(leg.descriptor.id);
            }
        }

        // Compensation: reverse-order undo of legs applied this run, for reversible legs
        // that registered a compensation. Irreversible legs are NOT compensated (cannot be
        // undone) — their presence in the report flags a manual-reconcile boundary.
        let mut compensated: Vec<NodeId> = Vec::new();
        if failure_at.is_some() {
            for &i in applied_this_run.iter().rev() {
                let leg = &legs[i];
                if leg.descriptor.irreversible {
                    continue;
                }
                if let Some(comp) = &leg.compensation {
                    applier.compensate(leg, comp);
                    compensated.push(leg.descriptor.id);
                }
            }
        }

        RecoveryReport::new(records, failure_at, compensated)
    }

    /// The single-source **ACID** path (RFD §6): the same per-leg idempotency rules, but a
    /// hard failure rolls the **whole** transaction back to zero applied effects (the driver's
    /// `BEGIN…ROLLBACK`), rather than running per-leg compensation. The caller drives the
    /// real `begin`/`commit`/`rollback`; here we apply the legs and report, signalling rollback
    /// via [`RecoveryReport::rolled_back`]. On any hard failure the report's `failure_at` is
    /// set and `rolled_back` is `true`, so the runtime issues the driver `rollback`.
    #[must_use]
    pub fn run_acid(&self, applier: &mut dyn LegApplier, legs: &[EffectLeg]) -> RecoveryReport {
        let mut records: Vec<LegRecord> = Vec::with_capacity(legs.len());
        let mut failure_at: Option<NodeId> = None;

        for leg in legs {
            if failure_at.is_some() {
                records.push(LegRecord::skipped(leg));
                continue;
            }
            let outcome = self.apply_leg(applier, leg);
            let hard_failure = matches!(outcome, LegOutcome::Failed(_) | LegOutcome::Conflict(_));
            records.push(LegRecord::from_outcome(leg, outcome));
            if hard_failure {
                failure_at = Some(leg.descriptor.id);
            }
        }

        let rolled_back = failure_at.is_some();
        RecoveryReport::new(records, failure_at, Vec::new()).rolled_back(rolled_back)
    }
}

/// Re-base a precondition on the version the world currently holds — the recovery step after a
/// `Conflict`. The next write is conditioned on this fresh version (`If-Version`), so the
/// retry is a genuine read-then-write, not a blind overwrite.
fn rebase_precondition(world_version: &Version) -> crate::version::Precondition {
    crate::version::Precondition::IfVersion(world_version.clone())
}

/// A convenience: whether a slice of outcomes represents a fully-applied (or fully-resumed)
/// saga — every leg is `Applied` or `AlreadyApplied`. Used by the runtime to decide commit.
#[must_use]
pub fn all_succeeded(outcomes: &[LegOutcome]) -> bool {
    outcomes.iter().all(LegOutcome::is_success)
}
