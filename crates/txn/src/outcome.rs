//! Per-leg outcomes and the secret-free descriptors the audit ledger records
//! (RFD-0001 §6 recovery, §10 audit — credentials and payloads never logged).

use cfs_plan::{EffectKind, NodeId, Target};
use serde::Serialize;

use crate::key::EffectKey;
use crate::version::{Precondition, Version};

/// A secret-free description of an intended effect — the **append-before-apply** record.
///
/// Records identity + shape (`what`, `where`, the idempotency key, the precondition guard),
/// **never** the row payload or any credential (RFD §10 — redact at this boundary). The
/// ledger writes this *before* the driver is touched so a crash leaves a reconstructable
/// intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EffectDescriptor {
    /// The plan-local node id.
    pub id: NodeId,
    /// The idempotency key (the ledger dedup handle).
    pub key: EffectKey,
    /// What the effect does.
    pub kind: EffectKind,
    /// Where it lands (driver + path — no secrets).
    pub target: Target,
    /// The optimistic-concurrency guard, if any.
    pub precondition: Precondition,
    /// Whether the effect is irreversible (drives the no-retry / no-compensate rule).
    pub irreversible: bool,
    /// How many rows the payload carries — a count only, never the payload itself.
    pub arg_rows: usize,
}

/// What a driver reports back for one applied effect — a secret-free **receipt**.
///
/// Carries the affected count and the new version coordinate the write produced (so a
/// follow-on read-then-write can chain its precondition). No payload, no credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EffectReceipt {
    /// The node that was applied.
    pub id: NodeId,
    /// How many rows / objects the apply touched.
    pub affected: u64,
    /// The new version coordinate after the write (for chaining), if the node is versioned.
    pub new_version: Option<Version>,
}

impl EffectReceipt {
    /// Construct a receipt with no version coordinate.
    #[must_use]
    pub fn new(id: NodeId, affected: u64) -> Self {
        Self {
            id,
            affected,
            new_version: None,
        }
    }

    /// Builder: attach the post-write version coordinate.
    #[must_use]
    pub fn with_version(mut self, v: Version) -> Self {
        self.new_version = Some(v);
        self
    }
}

/// A structured, machine-readable per-leg failure (RFD §6) — the saga/strategy counterpart
/// of the runtime `EffectError`, kept here so `cfs-txn` is self-contained and pure. The
/// `class`/`code` is the discriminant an AI agent (or the auto-retry loop) branches on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "class", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectError {
    /// A transient failure (rate limit, 5xx, timeout). Retryable on a non-irreversible leg.
    #[error("retryable effect failure: {reason}")]
    Retryable {
        /// A secret-free reason.
        reason: String,
    },
    /// A permanent failure (bad request, not found). No retry.
    #[error("terminal effect failure: {reason}")]
    Terminal {
        /// A secret-free reason.
        reason: String,
    },
}

impl EffectError {
    /// A short, stable machine code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            EffectError::Retryable { .. } => "retryable",
            EffectError::Terminal { .. } => "terminal",
        }
    }

    /// Whether this class is retryable (subject to the leg not being irreversible).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, EffectError::Retryable { .. })
    }

    /// Construct a retryable failure.
    #[must_use]
    pub fn retryable(reason: impl Into<String>) -> Self {
        EffectError::Retryable {
            reason: reason.into(),
        }
    }

    /// Construct a terminal failure.
    #[must_use]
    pub fn terminal(reason: impl Into<String>) -> Self {
        EffectError::Terminal {
            reason: reason.into(),
        }
    }
}

/// The outcome of applying one effect leg (RFD §6) — the closed set the saga/strategy
/// executors fold over. `AlreadyApplied` is the idempotent-resume no-op; `Conflict` carries
/// the version the world actually held so a bounded re-read can recover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LegOutcome {
    /// The effect applied for the first time, producing a receipt.
    Applied(EffectReceipt),
    /// The effect's [`EffectKey`] was already in the ledger — a no-op (idempotent resume /
    /// at-least-once redelivery). No second apply occurred.
    AlreadyApplied,
    /// The optimistic-concurrency guard failed: the world's version differs from the
    /// precondition. Carries the version the world actually holds (for a bounded re-read).
    Conflict(Version),
    /// The leg failed (after exhausting any retries on a retryable, reversible leg).
    Failed(EffectError),
}

impl LegOutcome {
    /// A short, stable machine code for the leg outcome.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            LegOutcome::Applied(_) => "applied",
            LegOutcome::AlreadyApplied => "already_applied",
            LegOutcome::Conflict(_) => "conflict",
            LegOutcome::Failed(_) => "failed",
        }
    }

    /// Whether this outcome counts as "the effect is now present in the world" — both a
    /// fresh apply and an `AlreadyApplied` no-op mean the world holds the effect, so a saga
    /// treats both as success and a re-run skips both.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, LegOutcome::Applied(_) | LegOutcome::AlreadyApplied)
    }
}
