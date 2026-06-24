//! [`GitError`] — the structured, secret-free error taxonomy for the git driver (RFD-0001 §5,
//! AI-consumable). Every variant is owned data the AI can act on; **no** object bytes, ref
//! names beyond the addressed one, or `.git/config` content ever leak here (RFD §10). The local
//! object model needs no credentials, so there is no credential surface to redact.

use thiserror::Error;

/// The git driver's error taxonomy. Maps to [`qfs_driver::CfsError`] at the contract boundary
/// and to a runtime [`qfs_runtime::EffectError`] in the apply leg.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitError {
    /// A `/git/<repo>[@<ref>]/<rest>` path could not be parsed into a node.
    #[error("invalid git path `{path}`: {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// Why it is invalid (AI feedback).
        reason: &'static str,
    },

    /// No repository is registered under the `<repo>` segment.
    #[error("no such repository `{repo}` is mounted")]
    UnknownRepo {
        /// The unresolved repo segment.
        repo: String,
    },

    /// A `@<ref>` temporal coordinate (branch/tag/sha/`HEAD~n`) did not resolve to an object.
    #[error("ref `{reference}` did not resolve to a commit/object")]
    UnresolvedRef {
        /// The unresolved ref expression.
        reference: String,
    },

    /// An object id was requested but is not present in the object database.
    #[error("object `{oid}` not found in the object database")]
    ObjectNotFound {
        /// The 40-char hex oid.
        oid: String,
    },

    /// A stored object (loose object / ref / reflog) is malformed.
    #[error("corrupt git data: {reason}")]
    Corrupt {
        /// The corruption cause (parser feedback).
        reason: String,
    },

    /// A capability-denied write (e.g. `UPDATE /commits`) reached a place a structural
    /// parse-time gate should have rejected — the apply-leg backstop.
    #[error("verb `{verb}` is not supported on `{path}`")]
    CapabilityDenied {
        /// The addressed path.
        path: String,
        /// The denied verb label.
        verb: &'static str,
    },

    /// A ref compare-and-swap failed: the ref's current oid did not match the expected
    /// `old` oid (optimistic concurrency, RFD §6). The write is **rejected, never clobbered**.
    #[error("ref `{name}` CAS conflict: expected old oid `{expected}`, found `{actual}`")]
    RefCasConflict {
        /// The ref being moved.
        name: String,
        /// The old oid the write asserted.
        expected: String,
        /// The oid actually present (the write is rejected, the ref untouched).
        actual: String,
    },

    /// An in-memory three-way merge/rebase computed a **conflict** during planning (PREVIEW).
    /// Surfaced as a typed plan-build error with ZERO effects — never a half-applied mutation.
    #[error("merge conflict in `{path}`: {reason}")]
    MergeConflict {
        /// The conflicting tree path.
        path: String,
        /// The conflict description (which sides diverged).
        reason: String,
    },

    /// A row/effect carried a malformed or missing column the write needs.
    #[error("malformed `{verb}` on `{path}`: {reason}")]
    MalformedEffect {
        /// The verb label.
        verb: &'static str,
        /// The addressed path.
        path: String,
        /// What was missing/wrong.
        reason: String,
    },
}

impl GitError {
    /// A short, stable error code for the structured `-json` surface and golden snapshots.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            GitError::InvalidPath { .. } => "invalid_path",
            GitError::UnknownRepo { .. } => "unknown_repo",
            GitError::UnresolvedRef { .. } => "unresolved_ref",
            GitError::ObjectNotFound { .. } => "object_not_found",
            GitError::Corrupt { .. } => "corrupt",
            GitError::CapabilityDenied { .. } => "capability_denied",
            GitError::RefCasConflict { .. } => "ref_cas_conflict",
            GitError::MergeConflict { .. } => "merge_conflict",
            GitError::MalformedEffect { .. } => "malformed_effect",
        }
    }
}

impl GitError {
    /// Map to the shared [`qfs_driver::CfsError`] at the contract boundary (`describe`).
    /// The workspace error enum has no generic "driver" arm (it is a closed, AI-facing set),
    /// so a path/ref/object error collapses to the structured `InvalidPath` arm carrying this
    /// driver's own message — secret-free by construction (the local object model has no
    /// credential surface).
    #[must_use]
    pub fn into_qfs(self, path: &str) -> qfs_driver::CfsError {
        qfs_driver::CfsError::InvalidPath {
            path: path.to_string(),
            reason: self.code(),
        }
    }
}
