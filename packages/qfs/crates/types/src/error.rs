//! [`TypeError`] — the **structured**, AI-consumable error of the type model (RFD §5
//! "structured error", matching the closed-core/structured-error policy of t04).
//!
//! Errors are sum-typed and carry enough context (available columns, offending type,
//! the operator and operand types) for an AI agent to *repair* the statement rather
//! than re-parse prose. Every arm has a stable [`TypeError::code`] the server/CLI can
//! branch on and surface in audit logs (RFD §10 observability).
//!
//! `qfs-types` is a leaf with no dependency on `qfs-driver`, so this does **not** use
//! the workspace `thiserror`-based `CfsError`; it is its own owned enum. The mapping
//! `TypeError → CfsError` is added at the `qfs-core` boundary in a later epic.

use serde::{Deserialize, Serialize};

use crate::predicate::CmpOp;
use crate::schema::{ColumnType, Name};

/// A structured type/schema error (RFD §5). `#[non_exhaustive]`: later epics add arms
/// as more type rules land, without breaking exhaustive matches in this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TypeError {
    /// A referenced column does not exist. Carries the available names so an AI can
    /// suggest the intended column (RFD §5).
    UnknownColumn {
        /// The name that failed to resolve.
        name: Name,
        /// The column names that *were* available, in order.
        available: Vec<Name>,
    },
    /// A dotted path descended into a non-`Struct` (and non-`Json`) column.
    NotAStruct {
        /// The path segment at which navigation failed.
        segment: String,
        /// The (non-struct) type encountered there.
        ty: ColumnType,
    },
    /// `EXPAND` targeted a column that is neither an `Array` nor a `Struct`.
    NotExpandable {
        /// The field that could not be expanded.
        field: Name,
        /// The (non-collection) type of that field.
        ty: ColumnType,
    },
    /// A comparison was applied to operands whose types are not comparable
    /// (e.g. `Int < Text`), or a text-only operator (`LIKE`/`~`) hit a non-`Text`
    /// operand.
    IncomparableTypes {
        /// The operator that failed to type-check.
        op: CmpOp,
        /// The left-hand-side type.
        lhs: ColumnType,
        /// The right-hand-side type.
        rhs: ColumnType,
    },
}

impl TypeError {
    /// A stable, machine-readable code (RFD §5). AI-facing callers branch on this
    /// rather than on a rendered message.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownColumn { .. } => "unknown_column",
            Self::NotAStruct { .. } => "not_a_struct",
            Self::NotExpandable { .. } => "not_expandable",
            Self::IncomparableTypes { .. } => "incomparable_types",
        }
    }
}

impl core::fmt::Display for TypeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownColumn { name, available } => {
                write!(f, "unknown column `{name}`; available: {available:?}")
            }
            Self::NotAStruct { segment, ty } => {
                write!(f, "cannot navigate into `{segment}`: not a struct ({ty:?})")
            }
            Self::NotExpandable { field, ty } => {
                write!(f, "cannot EXPAND `{field}`: not a collection ({ty:?})")
            }
            Self::IncomparableTypes { op, lhs, rhs } => {
                write!(f, "incomparable types for {op:?}: {lhs:?} vs {rhs:?}")
            }
        }
    }
}

impl std::error::Error for TypeError {}
