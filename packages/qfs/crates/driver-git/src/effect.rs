//! [`GitEffect`] — the effects-as-data (RFD §6) the git driver realises a write plan as.
//! **Writes are pure plans**: a write builds a DAG of these effects and applies **nothing**
//! until `COMMIT` drives the applier (the purity invariant). Three effect kinds:
//!
//! - [`GitEffect::WriteLooseObject`] — content-addressed object write. Idempotent (writing an
//!   existing oid is a no-op) → `irreversible: false` (the object is GC-able, never overwritten).
//! - [`GitEffect::UpdateRef`] — move a ref with **compare-and-swap on the old oid** (optimistic
//!   concurrency via `@version`, RFD §6): a stale `old` is rejected, never clobbered. A `force`
//!   move that orphans history is flagged but **reflog-recoverable**.
//! - [`GitEffect::WriteReflogEntry`] — append the recovery-oracle entry every applied ref move
//!   produces.
//!
//! These are owned DTOs; no `gix` type appears. The runtime [`EffectNode`] carries the row args
//! the driver decodes back into a `GitEffect` in the apply leg.

use crate::objectdb::{ObjectKind, Oid};

/// One fully-described git effect (effects-as-data). Built purely during planning; applied only
/// under `COMMIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitEffect {
    /// Write a loose object (blob/tree/commit/tag). Content-addressed → idempotent → reversible
    /// (GC-able), so `irreversible` is always false.
    WriteLooseObject {
        /// The content-addressed oid (`SHA-1(<type> <len>\0<payload>)`).
        oid: Oid,
        /// The object kind.
        kind: ObjectKind,
        /// The object payload (no header; the applier frames it).
        payload: Vec<u8>,
    },
    /// Move (or create) a ref with a compare-and-swap on the expected old oid.
    UpdateRef {
        /// The ref name (`refs/heads/main`, `refs/tags/v1`).
        name: String,
        /// The expected current oid (`None` = a creation; the ref must not yet exist). A
        /// mismatch is a [`crate::error::GitError::RefCasConflict`] — rejected, never clobbered.
        old: Option<Oid>,
        /// The new oid.
        new: Oid,
        /// Whether this is a forced move that may orphan history (flagged, reflog-recoverable).
        force: bool,
    },
    /// Append a reflog entry (the recovery oracle). Every applied ref move emits one.
    WriteReflogEntry {
        /// The ref the entry belongs to.
        name: String,
        /// The prior oid.
        old: Oid,
        /// The new oid.
        new: Oid,
        /// The actor identity line.
        who: String,
        /// The reflog message.
        message: String,
        /// The entry epoch seconds.
        time: i64,
    },
}

impl GitEffect {
    /// Whether applying this effect cannot be undone. Object writes are content-addressed and
    /// GC-able → reversible; ref moves (even forced) are reflog-recoverable → reversible. Nothing
    /// the git driver does is inherently irreversible (the deliberate safety win, RFD §6).
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        false
    }

    /// Whether this is a forced ref move that orphans history (PREVIEW warns; reflog recovers).
    #[must_use]
    pub const fn is_forced_ref_move(&self) -> bool {
        matches!(self, GitEffect::UpdateRef { force: true, .. })
    }

    /// A stable verb label for the audit ledger / preview.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            GitEffect::WriteLooseObject { .. } => "WRITE_OBJECT",
            GitEffect::UpdateRef { .. } => "UPDATE_REF",
            GitEffect::WriteReflogEntry { .. } => "WRITE_REFLOG",
        }
    }
}
