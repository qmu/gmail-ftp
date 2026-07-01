//! The [`CombineEngine`] seam and the in-house [`MiniEvaluator`] (ADR-0002).
//!
//! [`CombineEngine`] is the trait that keeps the DuckDB-vs-own decision reversible: a
//! `DuckDbEngine` could implement it behind a non-default feature without touching
//! callers. The shipped impl is [`MiniEvaluator`], which walks a
//! [`PhysicalPlan`](qfs_pushdown::PhysicalPlan), pulling one [`RowBatch`] per native
//! [`Scan`](qfs_pushdown::PhysicalPlan::Scan) from the supplied [`ScanResults`] and
//! folding each local [`Combine`](qfs_pushdown::PhysicalPlan::Combine) op over its inputs.

use qfs_pushdown::{CombineOp, PhysicalPlan};
use qfs_types::RowBatch;

use crate::eval;
use crate::scan::{Cursor, ScanResults};

/// A structured engine error. `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineError {
    /// The plan referenced more native scans than [`ScanResults`] supplied (a
    /// caller/runtime mismatch — the batcher must produce one batch per scan leaf).
    MissingScanResult {
        /// How many scan results were available.
        available: usize,
    },
    /// A binary combine op (`HashJoin`/set op) received other than two inputs.
    Arity {
        /// The op label that had the wrong input count.
        op: &'static str,
        /// The number of inputs actually supplied.
        inputs: usize,
    },
}

impl EngineError {
    /// A stable, machine-readable code (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            EngineError::MissingScanResult { .. } => "missing_scan_result",
            EngineError::Arity { .. } => "engine_arity",
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::MissingScanResult { available } => {
                write!(f, "missing scan result (only {available} available)")
            }
            EngineError::Arity { op, inputs } => {
                write!(f, "combine op `{op}` expected 2 inputs, got {inputs}")
            }
        }
    }
}

impl std::error::Error for EngineError {}

/// The local combine engine seam (ticket t14): execute a residual physical plan over the
/// native scan results, returning the final rows. Keeping this a trait is what makes the
/// engine choice (ADR-0002: own [`MiniEvaluator`]) reversible.
pub trait CombineEngine {
    /// Execute `plan` over `scans`, returning the combined [`RowBatch`].
    ///
    /// # Errors
    /// [`EngineError`] if the plan and scan results disagree on shape.
    fn execute(&self, plan: &PhysicalPlan, scans: ScanResults) -> Result<RowBatch, EngineError>;
}

/// The in-house relational evaluator (ADR-0002): a small, dependency-light, wasm-friendly
/// implementation of the residual operator set. The heavy lifting is pushed down, so this
/// only ever runs the cross-source remainder.
#[derive(Debug, Default, Clone, Copy)]
pub struct MiniEvaluator;

impl MiniEvaluator {
    /// Construct the evaluator (stateless).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl CombineEngine for MiniEvaluator {
    fn execute(&self, plan: &PhysicalPlan, scans: ScanResults) -> Result<RowBatch, EngineError> {
        let mut cursor = scans.into_cursor();
        eval_node(plan, &mut cursor)
    }
}

/// Recursively evaluate a physical node, pulling the next scan batch at each leaf.
fn eval_node(plan: &PhysicalPlan, cursor: &mut Cursor) -> Result<RowBatch, EngineError> {
    match plan {
        // A native scan: pull the next driver-produced batch. The batch is authoritative
        // for the rows; the `ScanNode.schema` is the planner's resolved schema, consumed
        // by the residual above, not re-applied here.
        PhysicalPlan::Scan(_scan) => cursor
            .next_batch()
            .ok_or(EngineError::MissingScanResult { available: 0 }),
        PhysicalPlan::Combine { op, inputs } => eval_combine(op, inputs, cursor),
    }
}

fn eval_combine(
    op: &CombineOp,
    inputs: &[PhysicalPlan],
    cursor: &mut Cursor,
) -> Result<RowBatch, EngineError> {
    match op {
        // Unary ops: one input.
        CombineOp::Filter(p) => Ok(eval::filter(unary(inputs, cursor)?, p)),
        CombineOp::Project(cols) => Ok(eval::project(unary(inputs, cursor)?, cols)),
        CombineOp::ProjectExpr(terms) => Ok(eval::project_expr(unary(inputs, cursor)?, terms)),
        CombineOp::Extend(asgns) => Ok(eval::extend(unary(inputs, cursor)?, asgns)),
        CombineOp::Limit(n) => Ok(eval::limit(unary(inputs, cursor)?, *n)),
        CombineOp::Sort(keys) => Ok(eval::sort(unary(inputs, cursor)?, keys)),
        CombineOp::Distinct => Ok(eval::distinct(unary(inputs, cursor)?)),
        CombineOp::Aggregate {
            group_by,
            aggregates,
        } => Ok(eval::aggregate(
            unary(inputs, cursor)?,
            group_by,
            aggregates,
        )),
        CombineOp::Expand(field) => Ok(eval::expand(unary(inputs, cursor)?, field)),
        // Binary ops: two inputs.
        CombineOp::HashJoin(on) => {
            let (l, r) = binary(inputs, cursor, "HashJoin")?;
            Ok(eval::hash_join(l, r, on))
        }
        CombineOp::SetOp(kind) => {
            let (l, r) = binary(inputs, cursor, kind.label())?;
            Ok(eval::set_op(l, r, *kind))
        }
    }
}

/// Evaluate the single child of a unary combine op.
fn unary(inputs: &[PhysicalPlan], cursor: &mut Cursor) -> Result<RowBatch, EngineError> {
    match inputs {
        [child] => eval_node(child, cursor),
        other => Err(EngineError::Arity {
            op: "unary",
            inputs: other.len(),
        }),
    }
}

/// Evaluate the two children of a binary combine op, **left-to-right** so the scan cursor
/// stays aligned with [`PhysicalPlan::scans`](qfs_pushdown::PhysicalPlan::scans).
fn binary(
    inputs: &[PhysicalPlan],
    cursor: &mut Cursor,
    op: &'static str,
) -> Result<(RowBatch, RowBatch), EngineError> {
    match inputs {
        [l, r] => {
            let left = eval_node(l, cursor)?;
            let right = eval_node(r, cursor)?;
            Ok((left, right))
        }
        other => Err(EngineError::Arity {
            op,
            inputs: other.len(),
        }),
    }
}
