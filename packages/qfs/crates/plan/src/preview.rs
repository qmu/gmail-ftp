//! `PREVIEW` rendering (RFD-0001 §6/§7/§10): the **dry-run** surface. [`preview`]
//! computes a [`Preview`] — a deterministic, secret-free summary of what a [`Plan`]
//! *would* do, with no side effects. It has [`std::fmt::Display`] (human text) and
//! `serde::Serialize` (CLI `-json`).
//!
//! `PREVIEW` performs **no** I/O and applies **nothing**; it only reads the plan. The
//! distinction from `COMMIT` is exactly this: `PREVIEW` returns a [`Preview`];
//! `COMMIT` runs the [`PlanApplier`](crate::PlanApplier).

use serde::Serialize;

use crate::ids::{Affected, NodeId, Target};
use crate::plan::Plan;
use crate::topo::topo_order;

/// One row of a [`Preview`]: a single planned effect, in topological order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct PreviewRow {
    /// The node id (stable address, RFD §6 observability).
    pub id: NodeId,
    /// The effect verb label (`INSERT`/`REMOVE`/`CALL`/…).
    pub verb: String,
    /// Where it lands (driver + virtual path; no secrets).
    pub target: Target,
    /// The estimated rows touched (honest: `Exact`/`AtMost`/`Unknown`).
    pub affected: Affected,
    /// Whether this effect is irreversible.
    pub irreversible: bool,
}

/// The result of a dry run: an ordered, deterministic, **secret-free** summary of a
/// plan. Safe to log (RFD §10). Deterministic ordering (topological + `NodeId`
/// tie-break) makes it golden-testable and diff-stable.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Preview {
    /// The planned effects, in deterministic topological order.
    pub rows: Vec<PreviewRow>,
    /// The node ids of every irreversible effect, called out explicitly (RFD §10).
    pub irreversible: Vec<NodeId>,
    /// The combined affected estimate across all effects (honest combination).
    pub total_affected: Affected,
    /// `true` if the plan is empty (a query-only statement has nothing to apply).
    pub is_pure: bool,
}

/// Compute the dry-run [`Preview`] of a plan. Pure: no I/O, no apply. If the plan is
/// cyclic (a construction bug), nodes are rendered in insertion order as a fallback so
/// the preview is still total and the cycle is surfaced by [`Plan::validate`] upstream.
#[must_use]
pub fn preview(plan: &Plan) -> Preview {
    let order =
        topo_order(plan).unwrap_or_else(|| plan.nodes.iter().map(|n| n.id).collect::<Vec<_>>());

    let mut rows = Vec::with_capacity(order.len());
    let mut irreversible = Vec::new();
    let mut total = Affected::Exact(0);

    for id in order {
        let Some(node) = plan.node(id) else { continue };
        if node.irreversible {
            irreversible.push(node.id);
        }
        total = total.combine(node.est_affected);
        rows.push(PreviewRow {
            id: node.id,
            verb: verb_label(node),
            target: node.target.clone(),
            affected: node.est_affected,
            irreversible: node.irreversible,
        });
    }

    Preview {
        is_pure: plan.nodes.is_empty(),
        rows,
        irreversible,
        total_affected: total,
    }
}

/// The rendered verb for a preview row — `CALL` includes the procedure id so the
/// preview is self-describing (e.g. `CALL mail.send`).
fn verb_label(node: &crate::node::EffectNode) -> String {
    match &node.kind {
        crate::node::EffectKind::Call(proc) => format!("CALL {}", proc.as_str()),
        other => other.label().to_string(),
    }
}

impl std::fmt::Display for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_pure {
            return writeln!(f, "PREVIEW: pure query — no effects to apply.");
        }
        writeln!(f, "PREVIEW: {} effect(s)", self.rows.len())?;
        for row in &self.rows {
            let mark = if row.irreversible { " (!)" } else { "" };
            writeln!(
                f,
                "  {} {} -> {} [affected {}]{}",
                row.id, row.verb, row.target, row.affected, mark
            )?;
        }
        if !self.irreversible.is_empty() {
            let ids: Vec<String> = self.irreversible.iter().map(NodeId::to_string).collect();
            writeln!(
                f,
                "  (!) irreversible: {} node(s) [{}]",
                self.irreversible.len(),
                ids.join(", ")
            )?;
        }
        write!(f, "  total affected: {}", self.total_affected)
    }
}
