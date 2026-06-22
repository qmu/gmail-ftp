//! The saga **leg** vocabulary (RFD-0001 §6): one applyable effect plus its idempotency
//! key, optimistic-concurrency guard, and optional compensation — and the synchronous
//! [`LegApplier`] seam the runtime adapts its async driver to.

use cfs_plan::EffectNode;
use serde::{Deserialize, Serialize};

use crate::key::EffectKey;
use crate::outcome::{EffectDescriptor, LegOutcome};
use crate::version::Precondition;

/// One unit of saga work: a fully-keyed, guarded effect ready to apply. Built from a plan
/// [`EffectNode`] via [`EffectLeg::from_node`], which derives the [`EffectKey`] and lifts the
/// node's redacted [`EffectDescriptor`]. Carries an optional [`Compensation`] for the saga's
/// reverse-order undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectLeg {
    /// The deterministic idempotency key (ledger dedup handle).
    pub key: EffectKey,
    /// The secret-free descriptor recorded to the ledger before apply.
    pub descriptor: EffectDescriptor,
    /// The reverse-order undo for this leg, if it is compensable. `None` for legs with no
    /// natural inverse (and always `None` in effect for irreversible legs — never run).
    pub compensation: Option<Compensation>,
}

impl EffectLeg {
    /// Build a leg from a plan node within `plan_id`, deriving the key and the redacted
    /// descriptor. `precondition` is the optimistic-concurrency guard captured at the read
    /// that produced the write (or [`Precondition::None`] for an unconditional write).
    #[must_use]
    pub fn from_node(plan_id: &str, node: &EffectNode, precondition: Precondition) -> Self {
        let key = EffectKey::derive(plan_id, node);
        let descriptor = EffectDescriptor {
            id: node.id,
            key: key.clone(),
            kind: node.kind.clone(),
            target: node.target.clone(),
            precondition,
            irreversible: node.irreversible,
            arg_rows: node.args.rows.len(),
        };
        Self {
            key,
            descriptor,
            compensation: None,
        }
    }

    /// Builder: register the reverse-order compensation for this leg (ignored for
    /// irreversible legs, which the saga never compensates).
    #[must_use]
    pub fn with_compensation(mut self, comp: Compensation) -> Self {
        self.compensation = Some(comp);
        self
    }
}

/// A declarative description of how to undo an applied leg — a secret-free directive the
/// [`LegApplier`] interprets (e.g. "delete the row this insert created", "restore the prior
/// version"). Owned data only; no closures (so it stays `Serialize`/`Clone`/`Debug` and can
/// itself be recorded to the ledger for crash recovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Compensation {
    /// Undo a create by deleting what was created (the inverse of `Insert`/`Upsert`/`Copy`).
    DeleteCreated,
    /// Undo an update by restoring the captured prior version coordinate.
    RestoreVersion {
        /// The version coordinate to roll back to.
        to: crate::version::Version,
    },
    /// No automatic undo is possible (best-effort: flag for manual reconcile). Used to mark
    /// a boundary the saga cannot cross without a human / follow-up plan.
    ManualReconcile,
}

impl Compensation {
    /// A short, stable machine code for previews / golden snapshots.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Compensation::DeleteCreated => "delete_created",
            Compensation::RestoreVersion { .. } => "restore_version",
            Compensation::ManualReconcile => "manual_reconcile",
        }
    }
}

/// The synchronous **apply seam** the saga/ACID executors drive (RFD §3 — the only impure
/// op, reached here through a trait so `cfs-txn` itself stays pure). The runtime implements
/// it by bridging its async `ApplyDriver` (block-on / pre-resolved results); tests supply an
/// in-memory fake. The applier owns the actual I/O, optimistic-concurrency check (compare the
/// world's version to `precondition`), and version-stamping the receipt.
pub trait LegApplier {
    /// Apply `leg` under `precondition`. The applier reads the world's current version,
    /// compares it to the precondition, and either writes (returning
    /// [`LegOutcome::Applied`] with a fresh receipt + new version) or returns
    /// [`LegOutcome::Conflict`] carrying the version the world actually holds. A transient
    /// fault is [`LegOutcome::Failed`] with a retryable error; a permanent one is terminal.
    fn apply(&mut self, leg: &EffectLeg, precondition: &Precondition) -> LegOutcome;

    /// Run the reverse-order undo `comp` for an already-applied `leg`. Best-effort: a failed
    /// compensation is recorded by the caller but does not itself unwind further (the saga
    /// reports the boundary). Default: a no-op for appliers that register no compensations.
    fn compensate(&mut self, leg: &EffectLeg, comp: &Compensation) {
        let _ = (leg, comp);
    }
}
