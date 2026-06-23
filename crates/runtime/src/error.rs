//! Structured, machine-readable (AI-consumable) error taxonomy for the interpreter
//! (RFD-0001 §6 observability, §5 structured-error path). Errors are owned DTOs with a
//! stable `code()` — never a string blob and never a vendor type — so an AI agent can
//! branch on the failure class (retryable vs terminal vs capability-denied) and recover.

use cfs_plan::NodeId;
use cfs_types::DriverId;
use serde::Serialize;

/// Why one effect leg failed — the per-effect error a driver (or the runtime's own gate)
/// surfaces. The variants are the **recovery-relevant classes** (RFD §6): the scheduler
/// retries only [`EffectError::Retryable`] legs (and only when the node is not
/// `irreversible`); [`EffectError::Terminal`] and [`EffectError::CapabilityDenied`] stop
/// that branch immediately. Owned, serializable for the `-json` ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "class", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectError {
    /// A transient failure (rate limit, timeout, 5xx). Safe to retry on a
    /// non-`irreversible` leg up to the configured bound.
    #[error("retryable effect failure: {reason}")]
    Retryable {
        /// A human-readable, secret-free reason (never a payload or token).
        reason: String,
    },
    /// A permanent failure (bad request, not found, conflict). No retry.
    #[error("terminal effect failure: {reason}")]
    Terminal {
        /// A human-readable, secret-free reason.
        reason: String,
    },
    /// The effect's driver/verb is not in the active [`CapabilitySet`](crate::CapabilitySet).
    /// Rejected by the runtime **before** dispatch (defense in depth; the parse-time gate
    /// is t13). Never retried.
    #[error("capability denied: driver `{}` cannot {verb}", driver.as_str())]
    CapabilityDenied {
        /// The driver the denied effect targeted.
        driver: DriverId,
        /// The verb label that was denied (e.g. `REMOVE`, `CALL`).
        verb: String,
    },
    /// The per-leg timeout elapsed before the driver returned. Treated as retryable on a
    /// non-`irreversible` leg (the call may have not landed) — but an `irreversible` leg
    /// is never retried even on timeout (RFD §6 idempotency).
    #[error("effect timed out after {millis}ms")]
    TimedOut {
        /// The elapsed budget, in milliseconds.
        millis: u64,
    },
    /// An optimistic-concurrency precondition failed: the world held a different version than
    /// the conditional write expected (an `If-Version`/`If-Match` mismatch, typically a 412).
    /// The driver carries the version the world **actually** holds so the txn bridge can
    /// surface a typed [`LegOutcome::Conflict`](cfs_txn::LegOutcome::Conflict) with the real
    /// coordinate (no guessing from reason text). Never retried — a re-read must precede any
    /// re-apply, so this is terminal for the leg (RFD §6 no-lost-update).
    #[error("optimistic-concurrency conflict: world holds version `{version}`")]
    Conflict {
        /// The version the world currently holds (secret-free, opaque token).
        version: String,
    },
    /// A driver's sandbox / least-privilege boundary rejected a path that resolved **outside**
    /// the permitted root (`..` traversal, an absolute jump, or a symlink canonicalising out of
    /// the mount; RFD §10). This is a distinct, security-relevant discriminant from a plain
    /// capability denial: the audit ledger must distinguish an attempted *sandbox-escape* from
    /// "I lacked permission". **No I/O is performed** on the rejected path. Never retried —
    /// terminal for the leg.
    #[error("sandbox escape rejected for {path:?} (resolved outside the permitted root)")]
    SandboxEscape {
        /// The offending path (secret-free; the path is intended to surface, file contents
        /// never are).
        path: String,
    },
}

impl EffectError {
    /// A short, stable machine code for the structured error / golden snapshots / AI
    /// recovery — the single discriminant an agent branches on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            EffectError::Retryable { .. } => "retryable",
            EffectError::Terminal { .. } => "terminal",
            EffectError::CapabilityDenied { .. } => "capability_denied",
            EffectError::TimedOut { .. } => "timed_out",
            EffectError::Conflict { .. } => "conflict",
            EffectError::SandboxEscape { .. } => "sandbox_escape",
        }
    }

    /// Whether the scheduler may retry this class of failure (subject to the node not
    /// being `irreversible` and the retry bound). Only transient classes are retryable.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            EffectError::Retryable { .. } | EffectError::TimedOut { .. }
        )
    }

    /// Construct a retryable failure with a secret-free reason.
    #[must_use]
    pub fn retryable(reason: impl Into<String>) -> Self {
        EffectError::Retryable {
            reason: reason.into(),
        }
    }

    /// Construct a terminal failure with a secret-free reason.
    #[must_use]
    pub fn terminal(reason: impl Into<String>) -> Self {
        EffectError::Terminal {
            reason: reason.into(),
        }
    }

    /// Construct an optimistic-concurrency conflict carrying the version the world holds.
    #[must_use]
    pub fn conflict(version: impl Into<String>) -> Self {
        EffectError::Conflict {
            version: version.into(),
        }
    }

    /// Construct a sandbox-escape rejection carrying the offending path — the security-relevant
    /// discriminant the audit ledger uses to distinguish an attempted escape from a plain
    /// capability denial.
    #[must_use]
    pub fn sandbox_escape(path: impl Into<String>) -> Self {
        EffectError::SandboxEscape { path: path.into() }
    }
}

/// A whole-commit failure the [`Interpreter`](crate::Interpreter) returns when it cannot
/// even begin or complete the walk. Per-effect failures do **not** surface here — they are
/// recorded in the [`Outcome`](crate::Outcome) ledger and skip their dependents; this is
/// reserved for structural problems with the plan or runtime itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "class", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyError {
    /// The plan is not a DAG (a construction bug): no topological order exists, so the
    /// runtime refuses to execute any effect rather than guess an order.
    #[error("plan is cyclic or references missing nodes; cannot schedule")]
    InvalidPlan,
    /// A node referenced in the schedule was missing from the plan (a construction bug;
    /// should be caught by `Plan::validate`). Carries the offending id.
    #[error("plan references unknown node {0}")]
    UnknownNode(NodeId),
}

impl ApplyError {
    /// A short, stable machine code for the structured error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ApplyError::InvalidPlan => "invalid_plan",
            ApplyError::UnknownNode(_) => "unknown_node",
        }
    }
}
