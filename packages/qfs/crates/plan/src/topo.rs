//! Deterministic topological ordering of a [`Plan`]'s nodes (RFD-0001 §6).
//!
//! `commit` and `preview` walk nodes in this order. It is **stable**: Kahn's algorithm
//! with a [`NodeId`]-sorted ready set, so the same plan yields an identical order every
//! run — golden-test friendly and reproducible for diff-based CI dry-runs.

use std::collections::BTreeMap;

use crate::ids::NodeId;
use crate::plan::Plan;

/// Return the node ids of `plan` in a deterministic topological order, or `None` if the
/// graph is cyclic. Within each "ready" layer, ids are emitted in ascending [`NodeId`]
/// order so the result is reproducible.
#[must_use]
pub fn topo_order(plan: &Plan) -> Option<Vec<NodeId>> {
    // In-degree per node and the adjacency (parent -> children), keyed by NodeId in a
    // BTreeMap so iteration is deterministic.
    let mut indegree: BTreeMap<NodeId, usize> = plan.nodes.iter().map(|n| (n.id, 0usize)).collect();
    let mut children: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();

    for (parent, child) in &plan.deps {
        // Edges to/from unknown nodes are a dangling-dep bug caught by validate(); be
        // defensive here and treat them as no-ops so topo stays total.
        if !indegree.contains_key(parent) || !indegree.contains_key(child) {
            continue;
        }
        if let Some(d) = indegree.get_mut(child) {
            *d += 1;
        }
        children.entry(*parent).or_default().push(*child);
    }

    // Ready set: all zero-indegree nodes, kept sorted by NodeId (BTreeMap keys).
    let mut ready: Vec<NodeId> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    ready.sort_unstable();

    let mut order = Vec::with_capacity(plan.nodes.len());
    while let Some(next) = ready.first().copied() {
        ready.remove(0);
        order.push(next);
        if let Some(kids) = children.get(&next) {
            // Sort children for determinism before relaxing their in-degree.
            let mut kids = kids.clone();
            kids.sort_unstable();
            for child in kids {
                if let Some(d) = indegree.get_mut(&child) {
                    *d -= 1;
                    if *d == 0 {
                        // Insert keeping `ready` sorted ascending by NodeId.
                        let pos = ready.partition_point(|x| *x < child);
                        ready.insert(pos, child);
                    }
                }
            }
        }
    }

    if order.len() == plan.nodes.len() {
        Some(order)
    } else {
        // Some nodes never reached zero in-degree => a cycle.
        None
    }
}
