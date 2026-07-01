//! The [`LogicalPlan`] — a pure-query relational tree built from the AST upstream
//! (O-t07-1: the planner owns its own IR rather than consuming the evaluator's
//! schema-threading `PlanSource`). One variant per closed-core query operator; **no
//! effect variants** (effects pass through the effect-plan, never this pass).
//!
//! Crucially, the expression-bearing nodes carry the **expression/predicate**, not just
//! a schema (O-t07-3): [`LogicalPlan::Filter`] carries a [`Predicate`], [`Join`] carries
//! its `on` predicate, [`Project`]/[`Aggregate`] carry the column/aggregator lists — so
//! the planner can decide what each driver can run natively.

use std::sync::Arc;

use qfs_types::{ColRef, Name, Predicate, Schema, Value};

/// A scalar expression evaluated **per row** on the read path (t92, generalised from the
/// t92 composite literals). This is the lowered, vendor-free form of the projection/extend
/// expressions the parser AST carries — kept in the planner IR so the engine can evaluate a
/// struct/array constructor over a row's columns without depending on the parser or stdlib.
///
/// The set is deliberately closed to the forms the engine can evaluate with only the type
/// model: a column reference (with `.`-navigation), a constant [`Value`], and the `[ ]`/`{ }`
/// constructors. A scalar `fn(...)` in a projection is NOT lowered here (it needs the stdlib
/// registry the engine does not depend on) — it stays a structured `LowerError`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarExpr {
    /// A column reference (positional lookup, `.`-navigation into a `Struct`).
    Col(ColRef),
    /// A constant value, constant-folded from a literal at lower time.
    Lit(Value),
    /// An array constructor `[ e1, e2, … ]` — each element evaluated per row.
    Array(Vec<ScalarExpr>),
    /// A struct constructor `{ name: e, … }` — each field value evaluated per row, names kept.
    Struct(Vec<(Name, ScalarExpr)>),
}

/// The mount/driver a subtree resolves to (RFD §6). An owned `Arc<str>` so a `SourceId`
/// is cheap to clone while tagging every node of a subtree. Two subtrees share a source
/// iff their `SourceId`s are equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub Arc<str>);

impl SourceId {
    /// Construct a source id from owned text.
    #[must_use]
    pub fn new(id: impl AsRef<str>) -> Self {
        Self(Arc::from(id.as_ref()))
    }

    /// The source id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How two relations are joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `JOIN … ON …` — inner equi/theta join.
    Inner,
}

/// A set operation kind (`UNION`/`EXCEPT`/`INTERSECT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetKind {
    /// `UNION` — distinct union of both sides' rows.
    Union,
    /// `EXCEPT` — rows in the left not in the right.
    Except,
    /// `INTERSECT` — rows in both sides.
    Intersect,
}

impl SetKind {
    /// A stable label for `explain()` golden output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            SetKind::Union => "Union",
            SetKind::Except => "Except",
            SetKind::Intersect => "Intersect",
        }
    }
}

/// An aggregation function over a column (the residual combine + pushdown surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregator {
    /// `COUNT(col)` — non-null count (or row count for `*`-like column).
    Count,
    /// `SUM(col)`.
    Sum,
    /// `MIN(col)`.
    Min,
    /// `MAX(col)`.
    Max,
    /// `ARRAY_AGG(col)` — collect the column's per-row values (in row order) into one
    /// `Array` value (t92). Single-column, so it fits the closed [`Aggregate`] shape with no
    /// change to the aggregate representation; it is never pushed to a driver (local only).
    ArrayAgg,
}

impl Aggregator {
    /// A stable label for `explain()` and output column naming.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Aggregator::Count => "count",
            Aggregator::Sum => "sum",
            Aggregator::Min => "min",
            Aggregator::Max => "max",
            Aggregator::ArrayAgg => "array_agg",
        }
    }
}

/// One aggregate term: `<agg>(<column>) [AS <output>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    /// The aggregation function.
    pub func: Aggregator,
    /// The input column the function aggregates.
    pub column: Name,
    /// The output column name.
    pub output: Name,
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    /// The column to sort by.
    pub column: Name,
    /// `true` for `DESC`.
    pub descending: bool,
}

/// The pushdown planner's logical IR (RFD §6). Pure-query operators only.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// A base relation from one source (`/driver/...`). The leaf the
    /// source-tagging pass keys on.
    Scan {
        /// Which source/driver this scan reads.
        source: SourceId,
        /// The full addressed VFS path the `FROM` named (`/driver/seg/seg`), retained so a
        /// read driver scan can navigate to the exact node, not just the mount root (t28). The
        /// `source` keys the registry/pushdown profile; `path` is the concrete address.
        path: String,
        /// The node's output schema (from the driver's pure `describe`, t07).
        schema: Schema,
    },
    /// A `WHERE` filter. Carries the **typed predicate** so the planner can decide
    /// whether the driver can push it down (O-t07-3).
    Filter {
        /// The filtered input.
        input: Box<LogicalPlan>,
        /// The filter predicate (typed IR, t05).
        predicate: Predicate,
    },
    /// A `SELECT` projection narrowing to the named columns (all items are plain columns —
    /// pushable to a driver that supports `project`).
    Project {
        /// The projected input.
        input: Box<LogicalPlan>,
        /// The projected column names, in order.
        columns: Vec<Name>,
    },
    /// A `SELECT` projection with at least one **computed** term (a struct/array constructor,
    /// t92). Each output column is `(alias, ScalarExpr)`, evaluated per row. Never pushed to a
    /// driver (a driver cannot build a struct) — always a local residual op.
    ProjectExpr {
        /// The projected input.
        input: Box<LogicalPlan>,
        /// The `(output name, expression)` projection terms, in order.
        projections: Vec<(Name, ScalarExpr)>,
    },
    /// `EXTEND`/`SET` adding or overwriting columns with computed values (t92). Each
    /// assignment is `(name, ScalarExpr)`, evaluated per row. A local residual op — never
    /// pushed. (Previously EXTEND was a schema-only pass-through with no runtime effect.)
    Extend {
        /// The extended input.
        input: Box<LogicalPlan>,
        /// The `(column name, expression)` assignments, in order.
        assignments: Vec<(Name, ScalarExpr)>,
    },
    /// A `LIMIT n` cap.
    Limit {
        /// The limited input.
        input: Box<LogicalPlan>,
        /// The row cap.
        n: u64,
    },
    /// `ORDER BY …`.
    Sort {
        /// The sorted input.
        input: Box<LogicalPlan>,
        /// The sort keys, in priority order.
        keys: Vec<OrderKey>,
    },
    /// `DISTINCT` deduplication.
    Distinct {
        /// The deduplicated input.
        input: Box<LogicalPlan>,
    },
    /// `GROUP BY` + `AGGREGATE`.
    Aggregate {
        /// The grouped input.
        input: Box<LogicalPlan>,
        /// The grouping columns (empty = whole-relation aggregate).
        group_by: Vec<Name>,
        /// The aggregate terms.
        aggregates: Vec<Aggregate>,
    },
    /// `EXPAND <field>` — explode a nested collection into rows.
    Expand {
        /// The expanded input.
        input: Box<LogicalPlan>,
        /// The collection field to explode.
        field: Name,
    },
    /// `JOIN <rhs> ON <on>` — carries the join predicate (O-t07-3: the `on` is not
    /// dropped). A join whose two sides resolve to different sources federates locally.
    Join {
        /// The join kind.
        kind: JoinKind,
        /// The left input.
        lhs: Box<LogicalPlan>,
        /// The right input.
        rhs: Box<LogicalPlan>,
        /// The `ON` predicate (an equi-join column pair, the only push/local case t14
        /// needs; a richer theta predicate stays local).
        on: JoinOn,
    },
    /// `UNION`/`EXCEPT`/`INTERSECT` — a set op over two relations.
    SetOp {
        /// Which set operation.
        kind: SetKind,
        /// The left input.
        lhs: Box<LogicalPlan>,
        /// The right input.
        rhs: Box<LogicalPlan>,
    },
}

/// A join condition: an equality between a left column and a right column
/// (`lhs.col = rhs.col`). The minimal, deterministic join surface t14 needs; this is
/// what a hash-join keys on and what a single-source join can push down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOn {
    /// The left-side join column.
    pub left: Name,
    /// The right-side join column.
    pub right: Name,
}

impl JoinOn {
    /// Construct an equi-join condition `left = right`.
    #[must_use]
    pub fn eq(left: impl Into<Name>, right: impl Into<Name>) -> Self {
        Self {
            left: left.into(),
            right: right.into(),
        }
    }
}

impl LogicalPlan {
    /// The single [`SourceId`] this subtree resolves to, or `None` if it spans more than
    /// one source. A node is a pushdown candidate iff this is `Some` — its entire
    /// subtree shares one source (RFD §6 maximal same-source subtree).
    #[must_use]
    pub fn single_source(&self) -> Option<SourceId> {
        match self {
            LogicalPlan::Scan { source, .. } => Some(source.clone()),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::ProjectExpr { input, .. }
            | LogicalPlan::Extend { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Expand { input, .. } => input.single_source(),
            LogicalPlan::Join { lhs, rhs, .. } | LogicalPlan::SetOp { lhs, rhs, .. } => {
                match (lhs.single_source(), rhs.single_source()) {
                    (Some(a), Some(b)) if a == b => Some(a),
                    _ => None,
                }
            }
        }
    }

    /// Convenience constructor for a base scan with an empty address (callers that have no
    /// concrete path — e.g. an inline `VALUES` synthetic source).
    #[must_use]
    pub fn scan(source: SourceId, schema: Schema) -> Self {
        LogicalPlan::Scan {
            source,
            path: String::new(),
            schema,
        }
    }

    /// Convenience constructor for a base scan carrying the concrete addressed VFS path (t28).
    #[must_use]
    pub fn scan_at(source: SourceId, path: impl Into<String>, schema: Schema) -> Self {
        LogicalPlan::Scan {
            source,
            path: path.into(),
            schema,
        }
    }
}
