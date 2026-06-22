//! `COMMIT` semantics (RFD-0001 §6): the [`PlanApplier`] seam — the **only** impure
//! operation in the effect substrate — and [`commit`], which walks a [`Plan`] in
//! topological order applying each node, respecting dependencies and recording
//! applied/skipped accounting in a [`CommitReport`].
//!
//! The plan itself is pure data; `commit` takes an `&mut A: PlanApplier` so that all
//! side effects (and all secrets) live behind the applier boundary, never in the plan.
//! E0/E1 ship [`RecordingApplier`] — a test double that performs **no I/O**, records
//! every call, and returns the node's declared [`Affected`] — so PREVIEW/COMMIT can be
//! exercised end-to-end with no live credentials and no network (acceptance criterion).

use crate::ids::{Affected, NodeId};
use crate::node::EffectNode;
use crate::plan::Plan;
use crate::topo::topo_order;

/// The outcome of applying one effect — what the applier reports back. Owned data; no
/// secrets. The audit ledger (RFD §10, deferred) reconstructs from these.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppliedEffect {
    /// The node that was applied.
    pub id: NodeId,
    /// How many rows / objects the apply actually touched (the applier's true count,
    /// which may refine a planned `AtMost`/`Unknown` estimate into an exact number).
    pub affected: u64,
}

impl AppliedEffect {
    /// Construct an applied-effect report. Provided because the struct is
    /// `#[non_exhaustive]`, so out-of-crate (E4) driver-backed [`PlanApplier`] impls
    /// cannot build one with a struct literal — they need a constructor to report a
    /// successful apply. Additive; the `#[non_exhaustive]` guarantee is preserved.
    #[must_use]
    pub fn new(id: NodeId, affected: u64) -> Self {
        Self { id, affected }
    }
}

/// An error from applying one effect. The applier owns the real error taxonomy (E4);
/// here it is an owned message so the plan crate stays vendor-free and `commit` can
/// account for failures without leaking a driver type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApplyError {
    /// The node that failed.
    pub id: NodeId,
    /// A human-readable, secret-free reason.
    pub reason: String,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "apply {} failed: {}", self.id, self.reason)
    }
}

impl std::error::Error for ApplyError {}

/// The **only** impure seam of the effect substrate (RFD §3 purity invariant): a sink
/// that actually applies one effect. `commit` is the sole caller; real driver-backed
/// impls land in E4. Keeping this a trait is what makes `commit` testable with no creds
/// and what keeps secrets out of [`Plan`].
pub trait PlanApplier {
    /// Apply a single effect node, returning what it touched.
    ///
    /// # Errors
    /// [`ApplyError`] if the effect could not be applied; `commit` then skips every
    /// node that (transitively) depends on this one.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError>;
}

/// A test double (RFD §6 / acceptance): records every applied node in call order and
/// returns the node's declared [`Affected`] as the touched count, performing **no
/// I/O**. Optionally configured to fail on specific node ids to exercise the
/// skip-dependents path.
#[derive(Debug, Default)]
pub struct RecordingApplier {
    /// The node ids `apply` was called on, in call order (the recorded call log).
    pub applied: Vec<NodeId>,
    /// Node ids configured to fail when applied.
    fail_on: Vec<NodeId>,
}

impl RecordingApplier {
    /// A recording applier that always succeeds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure this node id to fail when applied (to test dependency skipping).
    #[must_use]
    pub fn failing_on(mut self, id: NodeId) -> Self {
        self.fail_on.push(id);
        self
    }
}

impl PlanApplier for RecordingApplier {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        self.applied.push(node.id);
        if self.fail_on.contains(&node.id) {
            return Err(ApplyError {
                id: node.id,
                reason: "configured failure".to_string(),
            });
        }
        let affected = match node.est_affected {
            Affected::Exact(n) | Affected::AtMost(n) => n,
            Affected::Unknown => 0,
        };
        Ok(AppliedEffect {
            id: node.id,
            affected,
        })
    }
}

/// Why a node was not applied during `commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// A node this one depends on (transitively) failed, so it was not attempted.
    DependencyFailed(NodeId),
}

/// The accounting result of [`commit`]: which effects applied, which were skipped, and
/// any failure. Enough for a future recovery pass to reconstruct progress (RFD §6):
/// `commit` is re-runnable against a partially-applied plan because `applied` lists the
/// `NodeId`s already done.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CommitReport {
    /// The effects that applied successfully, in apply order.
    pub applied: Vec<AppliedEffect>,
    /// The effects skipped because a dependency failed, with the reason.
    pub skipped: Vec<(NodeId, SkipReason)>,
    /// The first apply failure, if any (commit stops attempting new roots after it,
    /// but still records dependents as skipped).
    pub failed: Option<ApplyError>,
}

impl CommitReport {
    /// Whether the commit applied every node with no failure.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_none() && self.skipped.is_empty()
    }
}

/// Apply a plan by walking it in deterministic topological order, calling `applier`
/// once per node and respecting dependencies. When a node fails, every node that
/// (transitively) depends on it is recorded **skipped** rather than applied — so the
/// recorded call log never contains a node whose parent failed.
///
/// `on_applied` is the single ledger/observability funnel (RFD §6/§10): it is invoked
/// with each [`AppliedEffect`] right after a successful apply, the hook a future audit
/// ledger attaches to. Pass a no-op closure when not needed.
///
/// A cyclic plan (a construction bug) yields an empty report; callers should
/// [`Plan::validate`] first. `commit` performs no I/O of its own — all side effects are
/// the applier's.
pub fn commit<A, F>(plan: &Plan, applier: &mut A, mut on_applied: F) -> CommitReport
where
    A: PlanApplier,
    F: FnMut(&AppliedEffect),
{
    let mut report = CommitReport::default();
    let Some(order) = topo_order(plan) else {
        return report;
    };

    // Node ids known to have failed (directly or via a failed dependency).
    let mut tainted: Vec<NodeId> = Vec::new();

    for id in order {
        let Some(node) = plan.node(id) else { continue };

        // If any parent of this node is tainted, skip it (do not call apply).
        let failed_parent = plan
            .deps
            .iter()
            .filter(|(_, child)| *child == id)
            .map(|(parent, _)| *parent)
            .find(|parent| tainted.contains(parent));

        if let Some(parent) = failed_parent {
            tainted.push(id);
            report
                .skipped
                .push((id, SkipReason::DependencyFailed(parent)));
            continue;
        }

        match applier.apply(node) {
            Ok(effect) => {
                on_applied(&effect);
                report.applied.push(effect);
            }
            Err(err) => {
                tainted.push(id);
                if report.failed.is_none() {
                    report.failed = Some(err);
                }
            }
        }
    }

    report
}
