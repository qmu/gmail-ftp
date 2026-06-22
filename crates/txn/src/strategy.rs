//! Commit-strategy selection (RFD-0001 §6) and the recoverable `cp`/`mv` triple.
//!
//! A plan whose every write touches **one** source whose driver supports real transactions
//! can run as a single ACID `BEGIN…COMMIT` ([`CommitStrategy::SingleSourceAcid`]); a plan
//! that spans sources has no distributed transaction, so it becomes an orchestrated
//! best-effort **saga** ([`CommitStrategy::CrossSourceSaga`]) with append-before-apply
//! recovery. The strategy is chosen **purely** by inspecting which [`DriverId`]s the plan's
//! write leaves touch — no I/O, so `PREVIEW` can show it.

use std::collections::BTreeSet;

use cfs_plan::{DriverId, EffectKind, Plan};
use serde::Serialize;

/// How a commit will be executed (RFD §6), chosen by inspecting the plan's write targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommitStrategy {
    /// Every write targets the single named source and its driver is [`Transactional`] —
    /// the commit runs as one ACID transaction (begin → apply legs → commit / rollback).
    SingleSourceAcid {
        /// The single source every write targets.
        source: DriverId,
    },
    /// Writes span multiple sources (or the single source is non-transactional) — no
    /// distributed transaction exists, so the commit is an orchestrated best-effort saga
    /// with per-leg compensation and an append-before-apply ledger for recovery.
    CrossSourceSaga,
}

impl CommitStrategy {
    /// A short, stable machine code for previews / golden snapshots.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CommitStrategy::SingleSourceAcid { .. } => "single_source_acid",
            CommitStrategy::CrossSourceSaga => "cross_source_saga",
        }
    }
}

/// Whether a driver declares real transaction support (RFD §6) — the saga-vs-ACID input the
/// planner consults alongside the single-source check. An owned bool query keyed by
/// [`DriverId`]; a driver absent from the set is saga-only (the conservative default).
///
/// This mirrors, at the orchestration layer, the optional `Transactional` super-capability a
/// `Driver` may declare; the runtime populates it from the registry so `cfs-txn` stays pure.
#[derive(Debug, Clone, Default)]
pub struct TransactionalDrivers {
    drivers: BTreeSet<DriverId>,
}

impl TransactionalDrivers {
    /// An empty set — every driver is saga-only.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Declare `driver` transactional (builder form).
    #[must_use]
    pub fn with(mut self, driver: DriverId) -> Self {
        self.drivers.insert(driver);
        self
    }

    /// Whether `driver` declared transaction support.
    #[must_use]
    pub fn supports(&self, driver: &DriverId) -> bool {
        self.drivers.contains(driver)
    }
}

/// Whether an [`EffectKind`] is a **write** (mutates the world) — only write targets
/// participate in single-source detection; `Read`/`List` dependencies are pure and ignored.
#[must_use]
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

/// Select the [`CommitStrategy`] for `plan` given which drivers are transactional. Pure: it
/// only inspects the plan's write-target [`DriverId`]s, so `PREVIEW` can render the choice
/// without executing. A plan whose every write hits one transactional source is ACID;
/// anything else (multiple sources, or a single non-transactional source) is a saga.
#[must_use]
pub fn select_strategy(plan: &Plan, transactional: &TransactionalDrivers) -> CommitStrategy {
    let mut sources: BTreeSet<DriverId> = BTreeSet::new();
    for node in plan.nodes() {
        if is_write(&node.kind) {
            sources.insert(node.target.driver.clone());
        }
    }
    let mut iter = sources.into_iter();
    match (iter.next(), iter.next()) {
        // Exactly one write source, and it is transactional → ACID.
        (Some(only), None) if transactional.supports(&only) => {
            CommitStrategy::SingleSourceAcid { source: only }
        }
        // Zero write sources (read-only plan), one non-transactional source, or many
        // sources → saga (the safe, recoverable default).
        _ => CommitStrategy::CrossSourceSaga,
    }
}

/// The three steps a cross-mount `cp`/`mv` compiles to (RFD §6, the canonical recoverable
/// pattern): **copy → verify → delete**, never delete-before-verify. A crash between steps is
/// recoverable from the ledger — `Verify` is idempotent and `Delete` is keyed, so a failed
/// delete leaves a harmless duplicate, **never** a hole (no data loss on the recoverable path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CpStep {
    /// Copy source → destination (idempotent under [`EffectKind::Upsert`] / keyed write).
    Copy,
    /// Verify the destination matches the source (idempotent read-back; the safety gate
    /// that must pass before any delete).
    Verify,
    /// Delete the source — **only** reached after `Verify` succeeds. Keyed so a re-run that
    /// crashed after copy completes the delete without re-copying.
    Delete,
}

impl CpStep {
    /// The recoverable `mv` step sequence in order — the triple a cross-mount move compiles
    /// to. A plain `cp` omits [`CpStep::Delete`] (it keeps the source); a `mv` runs all three.
    #[must_use]
    pub fn mv_sequence() -> [CpStep; 3] {
        [CpStep::Copy, CpStep::Verify, CpStep::Delete]
    }

    /// The `cp` step sequence (no delete — the source is preserved).
    #[must_use]
    pub fn cp_sequence() -> [CpStep; 2] {
        [CpStep::Copy, CpStep::Verify]
    }

    /// A short, stable label for previews / golden snapshots.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CpStep::Copy => "COPY",
            CpStep::Verify => "VERIFY",
            CpStep::Delete => "DELETE",
        }
    }
}
