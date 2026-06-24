//! The DAG **frontier scheduler** (RFD-0001 §6): yields successive ready-sets of effect
//! nodes whose dependencies are all satisfied, so the interpreter can batch and parallelise
//! a whole frontier at once (Haxl-style). This is the structural counterpart of
//! `qfs_plan::topo_order`, but *incremental* — it exposes one layer at a time and lets the
//! caller mark nodes done (or failed) as results arrive, which is what makes auto-batching
//! across the full frontier and skip-on-dependency-failure both correct under parallelism.

use std::collections::{BTreeMap, BTreeSet};

use qfs_plan::{NodeId, Plan};

/// Incremental topological frontier over a [`Plan`]. Construct with [`Frontier::new`]
/// (returns `None` for a cyclic plan), then repeatedly call [`Frontier::ready`] to get the
/// next batch of nodes with zero unresolved dependencies, and [`Frontier::complete`] /
/// [`Frontier::fail`] to advance. Nodes are emitted within a layer in ascending [`NodeId`]
/// order for determinism (matching `qfs_plan::topo_order`).
#[derive(Debug)]
pub struct Frontier {
    /// Remaining unresolved in-degree per node.
    indegree: BTreeMap<NodeId, usize>,
    /// parent -> children adjacency.
    children: BTreeMap<NodeId, Vec<NodeId>>,
    /// Nodes already handed out by `ready` and not yet completed/failed (in flight).
    dispatched: BTreeSet<NodeId>,
    /// Nodes whose result has been recorded (applied or failed).
    settled: BTreeSet<NodeId>,
    /// Nodes that failed (directly or transitively) — their dependents are skipped.
    tainted: BTreeSet<NodeId>,
    /// Total node count, to know when the walk is exhausted.
    total: usize,
}

/// A node surfaced by the frontier: its id plus the parent (if any) that tainted it. A
/// `None` cause means the node is ready to **execute**; a `Some(cause)` means the node must
/// be **skipped** because that parent failed (t09 semantics, surfaced incrementally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// The node's deps all applied — dispatch it.
    Run(NodeId),
    /// The node has a failed (transitive) dependency — skip it, recording `cause`.
    Skip {
        /// The node to skip.
        id: NodeId,
        /// The upstream node whose failure caused the skip.
        cause: NodeId,
    },
}

impl Frontier {
    /// Build a frontier over `plan`. Returns `None` if the plan is cyclic (no topological
    /// order) — the interpreter then refuses to run anything.
    #[must_use]
    pub fn new(plan: &Plan) -> Option<Self> {
        // A cyclic plan has no order; reuse the canonical check so behaviour matches commit.
        qfs_plan::topo_order(plan)?;

        let mut indegree: BTreeMap<NodeId, usize> =
            plan.nodes.iter().map(|n| (n.id, 0usize)).collect();
        let mut children: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for (parent, child) in &plan.deps {
            if !indegree.contains_key(parent) || !indegree.contains_key(child) {
                continue;
            }
            if let Some(d) = indegree.get_mut(child) {
                *d += 1;
            }
            children.entry(*parent).or_default().push(*child);
        }
        Some(Self {
            indegree,
            children,
            dispatched: BTreeSet::new(),
            settled: BTreeSet::new(),
            tainted: BTreeSet::new(),
            total: plan.nodes.len(),
        })
    }

    /// The next ready-set: every node with zero remaining in-degree that has not yet been
    /// dispatched or settled. Each entry is either [`Ready::Run`] (deps applied) or
    /// [`Ready::Skip`] (a dep failed). Returns an empty vec when nothing is currently ready
    /// (either all in flight, or the walk is done — distinguish via [`Frontier::is_done`]).
    ///
    /// Materialising the **whole** ready-set in one call is what lets the interpreter group
    /// the entire frontier before dispatch, so N independent same-kind effects collapse into
    /// one batched driver call (the N+1 → 1 property).
    pub fn ready(&mut self) -> Vec<Ready> {
        let mut out = Vec::new();
        let ids: Vec<NodeId> = self
            .indegree
            .iter()
            .filter(|(id, d)| {
                **d == 0 && !self.dispatched.contains(id) && !self.settled.contains(id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(cause) = self.tainting_parent(id) {
                // Mark skipped immediately and relax its children so the taint propagates.
                self.settled.insert(id);
                self.tainted.insert(id);
                self.relax_children(id);
                out.push(Ready::Skip { id, cause });
            } else {
                self.dispatched.insert(id);
                out.push(Ready::Run(id));
            }
        }
        out
    }

    /// The first parent of `id` that is tainted (failed transitively), if any. Drives the
    /// skip decision. Uses the recorded `children` adjacency reversed lazily — cheap for the
    /// shallow DAGs qfs plans produce.
    fn tainting_parent(&self, id: NodeId) -> Option<NodeId> {
        self.children
            .iter()
            .filter(|(_, kids)| kids.contains(&id))
            .map(|(parent, _)| *parent)
            .find(|parent| self.tainted.contains(parent))
    }

    /// Record that `id` applied successfully — relax its children's in-degree so the next
    /// [`Frontier::ready`] surfaces newly-unblocked nodes.
    pub fn complete(&mut self, id: NodeId) {
        if self.settled.insert(id) {
            self.dispatched.remove(&id);
            self.relax_children(id);
        }
    }

    /// Record that `id` failed — taint it so its dependents are skipped (not dispatched),
    /// preserving the t09 skip-dependents semantics under parallelism.
    pub fn fail(&mut self, id: NodeId) {
        if self.settled.insert(id) {
            self.dispatched.remove(&id);
            self.tainted.insert(id);
            self.relax_children(id);
        }
    }

    /// Decrement the in-degree of every child of `id` (it has now settled, success or fail).
    fn relax_children(&mut self, id: NodeId) {
        if let Some(kids) = self.children.get(&id).cloned() {
            for child in kids {
                if let Some(d) = self.indegree.get_mut(&child) {
                    *d = d.saturating_sub(1);
                }
            }
        }
    }

    /// Whether every node has settled (applied, failed, or skipped) — the walk is finished.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.settled.len() == self.total
    }
}
