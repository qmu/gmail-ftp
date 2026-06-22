//! [`PlanError`] — the structured, AI-consumable error of the pushdown planner (RFD §5).
//!
//! Capability/policy denial fails **at plan time** (RFD §5 parse-time rejection mirror),
//! never as a partial scan; an unknown source is its own arm. No credentials appear.

use crate::logical::SourceId;

/// A structured pushdown-planning error. `#[non_exhaustive]` so later epics can add
/// arms (cost-model rejections, policy denials) without breaking exhaustive matches.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub enum PlanError {
    /// A subtree resolved to a [`SourceId`] that is not registered. The planner cannot
    /// negotiate pushdown with an unknown source, so it rejects at plan time rather than
    /// emitting a scan that would fail at execution.
    UnknownSource {
        /// The unregistered source.
        source: String,
    },
    /// The source's capabilities deny an operation in its subtree (RFD §5/§10). Rejected
    /// at plan time — never a partial scan. Carries the source and the denied op label.
    CapabilityDenied {
        /// The source whose capabilities denied the op.
        source: String,
        /// A stable label for the denied operation (e.g. `SELECT`, `aggregate`).
        op: &'static str,
    },
}

impl PlanError {
    /// A stable, machine-readable code an AI-facing caller branches on (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            PlanError::UnknownSource { .. } => "unknown_source",
            PlanError::CapabilityDenied { .. } => "capability_denied",
        }
    }

    pub(crate) fn unknown_source(source: &SourceId) -> Self {
        PlanError::UnknownSource {
            source: source.as_str().to_string(),
        }
    }

    pub(crate) fn capability_denied(source: &SourceId, op: &'static str) -> Self {
        PlanError::CapabilityDenied {
            source: source.as_str().to_string(),
            op,
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::UnknownSource { source } => {
                write!(f, "unknown source `{source}`")
            }
            PlanError::CapabilityDenied { source, op } => {
                write!(f, "source `{source}` denies operation `{op}`")
            }
        }
    }
}

impl std::error::Error for PlanError {}
