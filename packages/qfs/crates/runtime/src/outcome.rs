//! The applied-effect **ledger** (RFD-0001 §6 recovery substrate, §10 audit). Every leg is
//! recorded here — id, driver, kind, status, irreversible flag, duration — so a crash can
//! be reconstructed and an audit trail produced. Owned, serializable (`-json`); records
//! effect **metadata only**, never payloads, credentials, or tokens (RFD §10).

use std::time::Duration;

use qfs_plan::{EffectKind, NodeId};
use qfs_types::DriverId;
use serde::Serialize;

use crate::error::EffectError;

/// What a driver reports back for one successfully applied effect — the apply-time
/// counterpart of `qfs_plan::AppliedEffect`, carrying the true affected count the driver
/// observed (which may refine a planned estimate). Owned data; no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EffectOutput {
    /// The node that was applied (echoes the input id for fan-out).
    pub id: NodeId,
    /// How many rows / objects the apply actually touched.
    pub affected: u64,
}

impl EffectOutput {
    /// Construct an apply result.
    #[must_use]
    pub fn new(id: NodeId, affected: u64) -> Self {
        Self { id, affected }
    }
}

/// The terminal disposition of one effect in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LegStatus {
    /// The effect applied successfully, touching `affected` rows/objects.
    Applied {
        /// The true affected count the driver reported.
        affected: u64,
        /// How many attempts it took (1 = first try; >1 = succeeded after retries).
        attempts: u32,
    },
    /// The effect was attempted but failed terminally (after exhausting any retries).
    Failed {
        /// The structured, machine-readable failure (its `class`/`code` drives recovery).
        error: EffectError,
        /// How many attempts were made before giving up.
        attempts: u32,
    },
    /// The effect was **never attempted** because a (transitive) dependency failed — the
    /// t09 skip-dependents semantics, preserved under parallelism.
    Skipped {
        /// The upstream node whose failure caused this skip.
        cause: NodeId,
    },
}

impl LegStatus {
    /// A short, stable machine code for the leg disposition.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            LegStatus::Applied { .. } => "applied",
            LegStatus::Failed { .. } => "failed",
            LegStatus::Skipped { .. } => "skipped",
        }
    }
}

/// One ledger entry: the full record of an effect's disposition (RFD §6/§10). Serializable
/// for `-json` audit output; metadata only — no payloads, no tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LedgerEntry {
    /// The plan-local node identity.
    pub id: NodeId,
    /// The driver the effect was routed to.
    pub driver: DriverId,
    /// What the effect did.
    pub kind: EffectKind,
    /// Whether the effect was irreversible (drove the no-retry rule).
    pub irreversible: bool,
    /// The terminal disposition (applied / failed / skipped).
    pub status: LegStatus,
    /// Wall-clock duration of the apply attempt(s). Zero for skipped legs and for the
    /// `PREVIEW` mode (which performs no apply).
    #[serde(serialize_with = "ser_millis")]
    pub duration: Duration,
}

/// Serialize a [`Duration`] as integer milliseconds for the stable `-json` ledger shape
/// (a float-free, deterministic projection — golden-test friendly).
fn ser_millis<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The whole-commit result the [`Interpreter`](crate::Interpreter) returns: the ordered
/// ledger plus roll-up accounting (RFD §6). The ledger is the recovery substrate — a
/// re-run can skip the `applied` ids. Deterministic order: entries are emitted in the
/// plan's stable topological order regardless of the parallel execution interleaving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[non_exhaustive]
pub struct Outcome {
    /// Every effect's disposition, in stable topological (not wall-clock) order.
    pub ledger: Vec<LedgerEntry>,
}

impl Outcome {
    /// The node ids that applied successfully — what a recovery re-run skips.
    #[must_use]
    pub fn applied_ids(&self) -> Vec<NodeId> {
        self.ledger
            .iter()
            .filter(|e| matches!(e.status, LegStatus::Applied { .. }))
            .map(|e| e.id)
            .collect()
    }

    /// Whether every effect applied (no failure, no skip).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.ledger
            .iter()
            .all(|e| matches!(e.status, LegStatus::Applied { .. }))
    }

    /// The number of effects that failed terminally.
    #[must_use]
    pub fn failed_count(&self) -> usize {
        self.ledger
            .iter()
            .filter(|e| matches!(e.status, LegStatus::Failed { .. }))
            .count()
    }

    /// The number of effects skipped because a dependency failed.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.ledger
            .iter()
            .filter(|e| matches!(e.status, LegStatus::Skipped { .. }))
            .count()
    }
}
