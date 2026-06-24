//! The per-commit **recovery report** (RFD-0001 §6 observability, §10 audit-of-record).
//!
//! Emitted by the saga / ACID executors and surfaced in the interpreter's `-json` output.
//! Records the disposition of every leg (applied / already-applied / conflict / failed /
//! skipped / compensated), where the commit failed (if it did), which legs were compensated
//! or whether the transaction rolled back. Owned, serializable, **secret-free** — the audit
//! trail, never payloads or credentials.

use qfs_plan::{EffectKind, NodeId, Target};
use serde::Serialize;

use crate::key::EffectKey;
use crate::leg::EffectLeg;
use crate::outcome::LegOutcome;

/// One leg's line in the recovery report — its identity + the [`LegOutcome`] it reached (or
/// `skipped` if a prior failure short-circuited the saga before it was attempted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LegRecord {
    /// The plan-local node id.
    pub id: NodeId,
    /// The idempotency key (so a re-run can correlate `AlreadyApplied`).
    pub key: EffectKey,
    /// What the leg did.
    pub kind: EffectKind,
    /// Where it landed.
    pub target: Target,
    /// Whether it was irreversible (drove no-retry / no-compensate).
    pub irreversible: bool,
    /// The disposition reached.
    pub outcome: LegOutcome,
}

impl LegRecord {
    /// Build a record from a leg + the outcome it reached.
    #[must_use]
    pub fn from_outcome(leg: &EffectLeg, outcome: LegOutcome) -> Self {
        Self {
            id: leg.descriptor.id,
            key: leg.key.clone(),
            kind: leg.descriptor.kind.clone(),
            target: leg.descriptor.target.clone(),
            irreversible: leg.descriptor.irreversible,
            outcome,
        }
    }

    /// Build a record for a leg the saga never attempted (a prior hard failure stopped it).
    /// Modelled as a terminal "skipped" failure so the report is total over every leg.
    #[must_use]
    pub fn skipped(leg: &EffectLeg) -> Self {
        Self {
            id: leg.descriptor.id,
            key: leg.key.clone(),
            kind: leg.descriptor.kind.clone(),
            target: leg.descriptor.target.clone(),
            irreversible: leg.descriptor.irreversible,
            outcome: LegOutcome::Failed(crate::outcome::EffectError::terminal(
                "skipped: a prior leg in the commit failed",
            )),
        }
    }
}

/// The whole-commit recovery report (RFD §6/§10). Deterministic leg order (plan order), so
/// it is golden-testable. The ledger is the durable audit-of-record; this is the per-commit
/// structured summary the runtime returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Every leg's disposition, in plan order.
    pub legs: Vec<LegRecord>,
    /// The node at which the commit hit its first hard failure, if any (`None` = clean).
    pub failure_at: Option<NodeId>,
    /// The legs that were compensated (reverse-order undo), newest-first.
    pub compensated: Vec<NodeId>,
    /// Whether the (single-source ACID) transaction was rolled back (no effects persisted).
    pub rolled_back: bool,
}

impl RecoveryReport {
    /// Assemble a report (saga form — `rolled_back` defaults `false`).
    #[must_use]
    pub fn new(legs: Vec<LegRecord>, failure_at: Option<NodeId>, compensated: Vec<NodeId>) -> Self {
        Self {
            legs,
            failure_at,
            compensated,
            rolled_back: false,
        }
    }

    /// Builder: set the ACID rollback flag.
    #[must_use]
    pub fn rolled_back(mut self, yes: bool) -> Self {
        self.rolled_back = yes;
        self
    }

    /// Whether the commit completed with no hard failure (every leg applied or resumed).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failure_at.is_none()
    }

    /// The number of legs that applied **fresh** this run (excludes idempotent no-ops).
    #[must_use]
    pub fn applied_count(&self) -> usize {
        self.legs
            .iter()
            .filter(|l| matches!(l.outcome, LegOutcome::Applied(_)))
            .count()
    }

    /// The number of legs that were idempotent no-ops (`AlreadyApplied`) — the resume /
    /// at-least-once redelivery count.
    #[must_use]
    pub fn already_applied_count(&self) -> usize {
        self.legs
            .iter()
            .filter(|l| matches!(l.outcome, LegOutcome::AlreadyApplied))
            .count()
    }

    /// The number of legs that hit an unrecovered optimistic-concurrency conflict.
    #[must_use]
    pub fn conflict_count(&self) -> usize {
        self.legs
            .iter()
            .filter(|l| matches!(l.outcome, LegOutcome::Conflict { .. }))
            .count()
    }

    /// The number of legs found `Indeterminate` on resume — an intent was recorded but the
    /// apply was never sealed (a crash window) and the leg was not replay-safe, so the
    /// reconcile pass refused to silently replay it (RFD §6/§10 apply-once). Each needs an
    /// `UPSERT`-style re-apply or operator confirmation.
    #[must_use]
    pub fn indeterminate_count(&self) -> usize {
        self.legs
            .iter()
            .filter(|l| matches!(l.outcome, LegOutcome::Indeterminate { .. }))
            .count()
    }
}
