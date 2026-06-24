//! Owned identifier/coordinate newtypes for the effect plan (RFD-0001 §9 owned
//! DTOs): [`NodeId`], [`ProcId`], [`VfsPath`], [`Target`], and [`Affected`].
//!
//! Every type here is **owned, vendor-free data** — no driver SDK handles, no
//! credentials, no I/O. `DriverId` is re-used from `qfs-types` (the canonical leaf)
//! rather than redefined, so the workspace sees one driver-identity type.

use serde::Serialize;

pub use qfs_types::DriverId;

/// A stable, plan-local identifier for an [`EffectNode`](crate::EffectNode).
///
/// Newtype over `u32`. Assigned densely as nodes are appended; used as the dependency
/// edge endpoint and as the deterministic tie-breaker in the topological order
/// (`topo` sorts by `NodeId` within a layer) so previews are golden-test stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The underlying index.
    #[must_use]
    pub fn index(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The name of a callable procedure for a [`Call`](crate::EffectKind::Call) effect —
/// the procedure-registry seam (RFD §3), e.g. `mail.send`. An owned string resolved
/// later (E1 capability resolution); the plan only carries the name and the
/// declared-irreversible bit the planner was handed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ProcId(pub String);

impl ProcId {
    /// Construct a procedure id from owned text.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The procedure id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A qfs virtual path the effect targets, e.g. `/mail/inbox`, `/s3/bucket/key`
/// (RFD §2.1 VFS). An **owned, opaque** string wrapper — deliberately NOT the
/// `qfs-driver::Path` type, because `qfs-driver` depends on `qfs-plan` and importing
/// it would create a cycle (the spine is `qfs-driver → qfs-plan → qfs-types`). E4
/// adapts between this and the driver `Path` at the boundary. Carries **no secrets**
/// — previews are safe to log (RFD §10).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct VfsPath(pub String);

impl VfsPath {
    /// Construct a virtual path from owned text.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VfsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an effect lands: a driver identity plus the virtual path within it. An owned
/// DTO carrying identity and location only — never a credential, token, or vendor
/// handle (RFD §9/§10), so it is safe to render in a preview.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Target {
    /// The driver the effect is routed to.
    pub driver: DriverId,
    /// The virtual path within that driver.
    pub path: VfsPath,
}

impl Target {
    /// Construct a target from a driver id and a virtual path.
    #[must_use]
    pub fn new(driver: DriverId, path: VfsPath) -> Self {
        Self { driver, path }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.driver.as_str(), self.path)
    }
}

/// An estimate of how many rows / objects an effect will touch (RFD §10 honest
/// previews). Be honest rather than fabricating exact counts: a `Remove` over a *set*
/// reports [`Affected::AtMost`] or [`Affected::Unknown`] because the count is not known
/// until apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Affected {
    /// A known exact count (e.g. an `INSERT` of `n` literal rows).
    Exact(u64),
    /// An upper bound (e.g. a `REMOVE` filtered over a set of at most `n`).
    AtMost(u64),
    /// Unknown until apply (e.g. an unbounded pipeline-sourced effect).
    Unknown,
}

impl Affected {
    /// Combine two affected estimates into the conservative sum for a whole-plan
    /// total. `Unknown` is contagious (an unknown component makes the total unknown);
    /// otherwise the bounds add and the looser (`AtMost`) kind dominates `Exact`.
    #[must_use]
    pub fn combine(self, other: Affected) -> Affected {
        match (self, other) {
            (Affected::Unknown, _) | (_, Affected::Unknown) => Affected::Unknown,
            (Affected::Exact(a), Affected::Exact(b)) => Affected::Exact(a.saturating_add(b)),
            (
                Affected::Exact(a) | Affected::AtMost(a),
                Affected::Exact(b) | Affected::AtMost(b),
            ) => Affected::AtMost(a.saturating_add(b)),
        }
    }
}

impl std::fmt::Display for Affected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Affected::Exact(n) => write!(f, "{n}"),
            Affected::AtMost(n) => write!(f, "<={n}"),
            Affected::Unknown => f.write_str("?"),
        }
    }
}
