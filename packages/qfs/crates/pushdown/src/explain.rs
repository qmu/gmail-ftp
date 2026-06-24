//! [`explain`] — the deterministic plan-dump for golden tests and the audit-friendly
//! plan record (RFD §6 observability). Rule-based partitioning means identical input ⇒
//! byte-identical output, so golden tests are stable.
//!
//! Format: an indented tree, two spaces per level. A `Scan` line names its source and
//! the pushed work (`pushed=[where, project(...), limit N, ...]`, or `pushed=[]` for a
//! bare scan); a `Combine` line names the op and recurses into its inputs.

use std::fmt::Write as _;

use crate::physical::{CombineOp, PhysicalPlan, PushedQuery};

/// Render a [`PhysicalPlan`] to a stable, indented plan-dump string.
#[must_use]
pub fn explain(plan: &PhysicalPlan) -> String {
    let mut out = String::new();
    write_node(&mut out, plan, 0);
    out
}

fn write_node(out: &mut String, plan: &PhysicalPlan, depth: usize) {
    let indent = "  ".repeat(depth);
    match plan {
        PhysicalPlan::Scan(scan) => {
            let _ = writeln!(
                out,
                "{indent}Scan[{}] pushed=[{}]",
                scan.source,
                render_pushed(&scan.pushed),
            );
        }
        PhysicalPlan::Combine { op, inputs } => {
            let _ = writeln!(out, "{indent}Combine[{}]", render_op(op));
            for input in inputs {
                write_node(out, input, depth + 1);
            }
        }
    }
}

/// Render the pushed-down work as a stable comma-separated list, in a fixed order so the
/// golden output is deterministic.
fn render_pushed(p: &PushedQuery) -> String {
    let mut parts: Vec<String> = Vec::new();
    if p.filter.is_some() {
        parts.push("where".to_string());
    }
    if let Some(cols) = &p.project {
        parts.push(format!("project({})", cols.join(",")));
    }
    if p.distinct {
        parts.push("distinct".to_string());
    }
    if !p.group_by.is_empty() {
        parts.push(format!("group_by({})", p.group_by.join(",")));
    }
    if !p.aggregates.is_empty() {
        let terms: Vec<String> = p
            .aggregates
            .iter()
            .map(|a| format!("{}({})", a.func, a.column))
            .collect();
        parts.push(format!("aggregate({})", terms.join(",")));
    }
    if !p.order.is_empty() {
        let keys: Vec<String> = p
            .order
            .iter()
            .map(|k| {
                if k.descending {
                    format!("{} desc", k.column)
                } else {
                    k.column.clone()
                }
            })
            .collect();
        parts.push(format!("order({})", keys.join(",")));
    }
    if let Some(n) = p.limit {
        parts.push(format!("limit {n}"));
    }
    parts.join(", ")
}

/// Render a combine op's label + its salient parameters (deterministic).
fn render_op(op: &CombineOp) -> String {
    match op {
        CombineOp::Filter(_) => "Filter".to_string(),
        CombineOp::Project(cols) => format!("Project({})", cols.join(",")),
        CombineOp::Limit(n) => format!("Limit {n}"),
        CombineOp::Sort(keys) => {
            let ks: Vec<String> = keys
                .iter()
                .map(|k| {
                    if k.descending {
                        format!("{} desc", k.column)
                    } else {
                        k.column.clone()
                    }
                })
                .collect();
            format!("Sort({})", ks.join(","))
        }
        CombineOp::Distinct => "Distinct".to_string(),
        CombineOp::Aggregate {
            group_by,
            aggregates,
        } => {
            let aggs: Vec<String> = aggregates
                .iter()
                .map(|a| format!("{}({})", a.func.label(), a.column))
                .collect();
            if group_by.is_empty() {
                format!("Aggregate({})", aggs.join(","))
            } else {
                format!("Aggregate(by {}: {})", group_by.join(","), aggs.join(","))
            }
        }
        CombineOp::Expand(field) => format!("Expand({field})"),
        CombineOp::HashJoin(on) => format!("HashJoin({} = {})", on.left, on.right),
        CombineOp::SetOp(kind) => kind.label().to_string(),
    }
}
