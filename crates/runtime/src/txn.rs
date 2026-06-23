//! Interpreter integration of the transactional envelope (t11, RFD-0001 §6/§10).
//!
//! The pure orchestration policy lives in `cfs-txn` (idempotency keys, optimistic-concurrency
//! preconditions, commit-strategy selection, the saga/ACID executors, the audit ledger, the
//! recovery report). This module is the **async bridge**: it walks the plan's write legs in
//! plan order, applies each through the registry's async [`ApplyDriver`] honoring the leg's
//! [`Precondition`] (optimistic concurrency) and the [`AuditLedger`] (idempotency / resume),
//! and assembles a [`RecoveryReport`] dispatched on the [`CommitStrategy`].
//!
//! The apply loop is sequential (a saga is an ordered sequence with compensation; an ACID
//! group rolls back on first failure) — so it is fully deterministic and reuses the same
//! taint/skip discipline as the batched `commit`, while the wide-frontier batch/parallel path
//! (t10 `Interpreter::commit`) remains for the non-transactional bulk case.

use std::sync::Arc;

use cfs_plan::{EffectKind, EffectNode, Plan};
use cfs_txn::{
    select_strategy, AuditLedger, CommitStrategy, EffectError as TxnError, EffectKey, EffectLeg,
    EffectReceipt, LegOutcome, LegRecord, Precondition, RecoveryReport, TransactionalDrivers,
};

use crate::caps::CapabilitySet;
use crate::driver::{ApplyCx, DriverRegistry, EffectInput};
use crate::error::{ApplyError, EffectError};
use crate::interpreter::Interpreter;

/// Internal accessor so the t11 integration (this module) can resolve drivers from the
/// interpreter's registry without exposing the field. Kept crate-private.
impl Interpreter {
    pub(crate) fn drivers_ref(&self) -> &DriverRegistry {
        self.drivers_arc()
    }
}

/// How a single write leg is conditioned for optimistic concurrency — captured from the read
/// that produced it (E1/E4 thread this onto the node). At E0 the interpreter accepts an
/// explicit map from node id to precondition so tests and the evaluator can drive it without a
/// new plan-node field landing before the evaluator is ready to populate it.
pub type Preconditions = std::collections::HashMap<cfs_plan::NodeId, Precondition>;

impl Interpreter {
    /// **Transactional COMMIT** (t11): select the [`CommitStrategy`] for `plan`, then apply its
    /// write legs through the async driver registry with idempotency (the [`AuditLedger`] dedups
    /// retries / resumes) and optimistic concurrency (each leg's [`Precondition`] is honored),
    /// returning a [`RecoveryReport`]. `plan_id` seeds the deterministic [`EffectKey`]s;
    /// `preconditions` supplies the per-node optimistic-concurrency guards (empty = all
    /// unconditional); `transactional` declares which drivers support real transactions.
    ///
    /// On the ACID path a hard failure stops the walk and flags `rolled_back` (the caller's
    /// driver issues the real `ROLLBACK`); on the saga path applied legs would be compensated
    /// in reverse (compensation directives are E4-supplied; at E0 the report records the
    /// failure boundary and the ledger enables a recovering re-run).
    ///
    /// # Errors
    /// [`ApplyError::InvalidPlan`] if the plan is not a DAG.
    pub async fn commit_txn(
        &self,
        plan: &Plan,
        caps: &CapabilitySet,
        plan_id: &str,
        preconditions: &Preconditions,
        transactional: &TransactionalDrivers,
        ledger: &dyn AuditLedger,
    ) -> Result<(CommitStrategy, RecoveryReport), ApplyError> {
        // Order is the plan's stable topological order, so the recovery report is deterministic
        // regardless of wall-clock interleaving (matches the batched commit's `assemble`).
        let order = cfs_plan::topo_order(plan).ok_or(ApplyError::InvalidPlan)?;
        let strategy = select_strategy(plan, transactional);

        // Observability root span (RFD §6): one trace id per execution, threaded through every
        // per-leg child span so every applied-effect log line carries `trace_id` + `plan_id`.
        // Metadata only — never a payload or credential (RFD §10).
        let trace_id = crate::observe::TraceId::mint(plan_id);
        let span = tracing::info_span!(
            "commit_txn",
            trace_id = %trace_id,
            plan_id = %plan_id,
            strategy = %strategy.code(),
        );
        let _enter = span.enter();

        let mut records: Vec<LegRecord> = Vec::new();
        let mut failure = false;

        for id in &order {
            let Some(node) = plan.node(*id) else {
                continue;
            };
            // Only write effects are transactional legs; Read/List dependencies are pure and
            // are not recorded as legs (they carry no idempotency/precondition concern).
            if !is_write(&node.kind) {
                continue;
            }
            let precondition = preconditions.get(id).cloned().unwrap_or(Precondition::None);
            let leg = EffectLeg::from_node(plan_id, node, precondition.clone());

            if failure {
                // After a hard failure on either strategy, the remaining legs are not attempted
                // (ACID: the txn is rolling back; saga: the executor stops and compensates).
                records.push(LegRecord::skipped(&leg));
                continue;
            }

            let outcome = self
                .apply_txn_leg(node, caps, plan_id, &precondition, ledger)
                .await;
            // Structured audit event per leg: identity + outcome code + irreversibility, all
            // secret-free (RFD §10). Every line carries the inherited `trace_id`/`plan_id` from
            // the root span plus this leg's `effect.id`.
            tracing::info!(
                effect.id = node.id.index(),
                effect.driver = %node.target.driver.as_str(),
                effect.kind = %node.kind.label(),
                effect.irreversible = node.irreversible,
                outcome = %outcome.code(),
                "leg applied"
            );
            if matches!(
                outcome,
                LegOutcome::Failed(_)
                    | LegOutcome::Conflict { .. }
                    | LegOutcome::Indeterminate { .. }
            ) {
                failure = true;
            }
            records.push(LegRecord::from_outcome(&leg, outcome));
        }

        let failure_at = failure
            .then(|| records.iter().find(|r| is_hard(&r.outcome)).map(|r| r.id))
            .flatten();
        let rolled_back = failure && matches!(strategy, CommitStrategy::SingleSourceAcid { .. });
        let report = RecoveryReport::new(records, failure_at, Vec::new()).rolled_back(rolled_back);
        Ok((strategy, report))
    }

    /// Apply one write leg through the async driver, with the idempotency dedup + optimistic-
    /// concurrency precondition honored. Returns the [`LegOutcome`] (and seals the ledger on a
    /// fresh apply). Capability denial and missing driver map to terminal failures.
    async fn apply_txn_leg(
        &self,
        node: &EffectNode,
        caps: &CapabilitySet,
        plan_id: &str,
        precondition: &Precondition,
        ledger: &dyn AuditLedger,
    ) -> LegOutcome {
        // Per-leg child span (RFD §6): inherits the root `trace_id`/`plan_id` and adds this
        // leg's identity, so every log line emitted while the leg applies carries all three
        // ids. Metadata only — driver + path + kind, never a payload or credential (RFD §10).
        let leg_span = tracing::info_span!(
            "effect",
            effect.id = node.id.index(),
            effect.driver = %node.target.driver.as_str(),
            effect.path = %node.target.path.as_str(),
            effect.kind = %node.kind.label(),
        );
        let _leg_enter = leg_span.enter();

        let key = EffectKey::derive(plan_id, node);
        // Idempotent resume / at-least-once dedup: a prior *sealed* apply makes this a no-op.
        if ledger.applied(&key).is_some() {
            return LegOutcome::AlreadyApplied;
        }
        let leg = EffectLeg::from_node(plan_id, node, precondition.clone());
        // Crash-window reconcile (t12): an intent recorded with NO matching `applied` means a
        // prior run crashed between `record_intent` and `mark_applied` — the apply outcome is
        // ambiguous (the side effect may or may not have landed). Re-applying is only safe for
        // a naturally idempotent leg (`UPSERT`, or a conditionally-guarded write that would
        // catch a stale replay as a `Conflict`). A non-idempotent `Insert`/`Call`/`Remove`
        // must NOT be blindly replayed (RFD §6/§10 apply-once): surface it as `Indeterminate`
        // for `UPSERT`-style re-apply or operator confirmation instead.
        if ledger.has_intent(&key) && !leg.descriptor.is_replay_safe() {
            tracing::warn!(
                effect.key = %key,
                effect.kind = %node.kind.label(),
                "indeterminate effect: unsealed intent found on resume; refusing silent replay"
            );
            return LegOutcome::Indeterminate { key };
        }
        // Defense-in-depth capability re-check (same gate as the batched commit).
        if !caps.allows(&node.target, &node.kind) {
            return LegOutcome::Failed(TxnError::terminal(format!(
                "capability denied: driver `{}` cannot {}",
                node.target.driver.as_str(),
                node.kind.label()
            )));
        }
        // Append-before-apply: a crash after this leaves a reconstructable intent.
        ledger.record_intent(&key, &leg.descriptor);

        let Some(driver) = self.driver_for(&node.target.driver) else {
            return LegOutcome::Failed(TxnError::terminal(format!(
                "no driver registered for `{}`",
                node.target.driver.as_str()
            )));
        };

        let input = EffectInput::from_node(node);
        let cx = ApplyCx { last_attempt: true };
        let results = driver.apply_batch(node.kind.clone(), &[input], &cx).await;
        match results.into_iter().next() {
            Some(Ok(out)) => {
                let receipt = EffectReceipt::new(out.id, out.affected);
                ledger.mark_applied(&key, &receipt);
                LegOutcome::Applied(receipt)
            }
            Some(Err(e)) => map_effect_error(e, precondition),
            None => LegOutcome::Failed(TxnError::terminal("driver returned no result for effect")),
        }
    }

    /// Resolve the async driver for a [`DriverId`] from the registry (test/internal helper).
    fn driver_for(&self, id: &cfs_types::DriverId) -> Option<Arc<dyn crate::driver::ApplyDriver>> {
        self.drivers_ref().get(id)
    }
}

/// Map a runtime [`EffectError`] to a transactional [`LegOutcome`]. A typed
/// [`EffectError::Conflict`] (the driver observed the world's real version on an
/// optimistic-concurrency miss) is surfaced as a typed [`LegOutcome::Conflict`] carrying that
/// **real** world version — never inferred from reason text. Everything else stays a `Failed`.
/// `precondition` is retained for context (the conflict is only meaningful on a conditional
/// write) but the world version comes from the driver, not the expected token.
fn map_effect_error(e: EffectError, precondition: &Precondition) -> LegOutcome {
    match e {
        // The driver carried the world's actual version (t12): surface it directly so the
        // saga's bounded re-read sees the true coordinate, not the expected one.
        EffectError::Conflict { version } => {
            // A conflict on an unconditional write is a driver/contract bug; record it as a
            // terminal failure rather than a typed conflict (there is no precondition to
            // reconcile against), but still preserve the world version in the reason.
            if precondition.is_conditional() {
                LegOutcome::Conflict {
                    version: cfs_txn::Version::new(version),
                }
            } else {
                LegOutcome::Failed(TxnError::terminal(format!(
                    "unexpected conflict (no precondition) at world version `{version}`"
                )))
            }
        }
        EffectError::Retryable { reason } => LegOutcome::Failed(TxnError::retryable(reason)),
        EffectError::Terminal { reason } => LegOutcome::Failed(TxnError::terminal(reason)),
        EffectError::CapabilityDenied { driver, verb } => {
            LegOutcome::Failed(TxnError::terminal(format!(
                "capability denied: driver `{}` cannot {verb}",
                driver.as_str()
            )))
        }
        // A sandbox-escape is a terminal security failure for the leg — preserve the distinct
        // reason so the audit ledger keeps it apart from a plain capability denial.
        EffectError::SandboxEscape { path } => LegOutcome::Failed(TxnError::terminal(format!(
            "sandbox escape rejected for {path:?}"
        ))),
        EffectError::TimedOut { millis } => LegOutcome::Failed(TxnError::retryable(format!(
            "effect timed out after {millis}ms"
        ))),
    }
}

/// Whether an [`EffectKind`] mutates the world (a transactional leg). `Read`/`List` are pure.
fn is_write(kind: &EffectKind) -> bool {
    matches!(
        kind,
        EffectKind::Insert
            | EffectKind::Upsert
            | EffectKind::Update
            | EffectKind::Remove
            | EffectKind::Call(_)
    )
}

/// Whether a leg outcome is a hard failure (stops the strategy walk). An `Indeterminate`
/// outcome (unsealed-intent crash window we refused to replay) is hard: the saga cannot
/// safely proceed past an effect whose commit is ambiguous.
fn is_hard(outcome: &LegOutcome) -> bool {
    matches!(
        outcome,
        LegOutcome::Failed(_) | LegOutcome::Conflict { .. } | LegOutcome::Indeterminate { .. }
    )
}
