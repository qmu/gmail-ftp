//! Optimistic-concurrency coordinates (RFD-0001 §4 `@version`, §6 read-then-write).
//!
//! These are **owned DTOs** — a vendor SDK's ETag/`versionId`/git-ref type is converted to
//! one of these at the driver boundary (B3), so no `reqwest`/SDK type ever appears in a
//! `qfs-txn` signature. A read captures the coordinate it observed; the write that depends
//! on it carries the captured coordinate as a [`Precondition`] so a concurrent mutation is
//! caught as a structured [`Conflict`](crate::LegOutcome::Conflict) instead of a lost update.

use serde::{Deserialize, Serialize};

/// An opaque monotonic version coordinate (git ref, S3 `versionId`, Drive revision id).
///
/// The engine treats it as an **opaque, comparable token**: it never parses the inner
/// string, only compares two coordinates for equality (the optimistic-concurrency check).
/// Owned text — never a vendor handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version(pub String);

impl Version {
    /// Construct a version coordinate from owned text.
    #[must_use]
    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }

    /// The coordinate as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An opaque HTTP entity-tag captured at read for an `If-Match` conditional write.
///
/// Distinct from [`Version`] because the wire mechanism differs (`If-Match: <etag>` header
/// vs an expected-version query parameter), but both are owned, vendor-free tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Etag(pub String);

impl Etag {
    /// Construct an ETag from owned text.
    #[must_use]
    pub fn new(e: impl Into<String>) -> Self {
        Self(e.into())
    }

    /// The ETag as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Etag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The optimistic-concurrency guard attached to a write effect (RFD §6).
///
/// Captured at the read that produced the row and threaded onto the write node, so the
/// version travels **on the effect node**, never in interpreter-global state — the batch /
/// parallel reorder from t10 cannot lose or cross-wire it. `None` is an unconditional write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Precondition {
    /// No guard — an unconditional write (the only safe shape when the driver does not
    /// version the node, [`VersionSupport::None`](qfs_plan-adjacent driver declaration)).
    #[default]
    None,
    /// The write must observe exactly this version, else a [`Conflict`](crate::LegOutcome::Conflict).
    IfVersion(Version),
    /// The write must match this ETag (`If-Match: <etag>`), else a `Conflict`.
    IfMatchEtag(Etag),
}

impl Precondition {
    /// Whether this precondition guards the write at all (`false` only for [`Precondition::None`]).
    #[must_use]
    pub fn is_conditional(&self) -> bool {
        !matches!(self, Precondition::None)
    }

    /// The `If-Match` header value a driver would send for an ETag/version guard — the
    /// owned token the golden test asserts. `None` for an unconditional write.
    #[must_use]
    pub fn if_match_header(&self) -> Option<&str> {
        match self {
            Precondition::None => None,
            Precondition::IfVersion(v) => Some(v.as_str()),
            Precondition::IfMatchEtag(e) => Some(e.as_str()),
        }
    }

    /// Check an observed coordinate against this precondition. An unconditional write always
    /// passes; a conditional write passes iff the observed token equals the expected one.
    /// `observed` is the coordinate the world currently holds (what the driver read back).
    #[must_use]
    pub fn is_satisfied_by(&self, observed: &Version) -> bool {
        match self {
            Precondition::None => true,
            Precondition::IfVersion(v) => v == observed,
            // An ETag is compared structurally as an opaque token; the driver maps its own
            // ETag form to a Version coordinate for the world-state comparison.
            Precondition::IfMatchEtag(e) => e.as_str() == observed.as_str(),
        }
    }
}
