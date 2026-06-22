//! The [`PhysicalPlan`] — the output of [`partition_by_source`](crate::partition_by_source):
//! native [`ScanNode`]s (one per maximal pushed-down same-source subtree) combined by
//! local [`CombineOp`] residual operators.
//!
//! [`PushedQuery`] is an **owned engine-side DTO** describing the work pushed to a driver
//! (predicates / projection / limit / order / aggregate / …) — never a vendor query
//! object. The driver later translates it to SQL or plumbing inside its own boundary
//! (RFD §9, no vendor leak).

use cfs_types::{Name, Predicate, Schema};

use crate::logical::{Aggregate, JoinOn, OrderKey, SetKind, SourceId};

/// The owned description of the work pushed down to one source (RFD §6/§9). Each field
/// is what the driver accepted; the planner populated only the fields the driver's
/// [`PushdownProfile`](cfs_driver::PushdownProfile) declared it `supports_*`. Anything
/// the driver could not take stays in the local residual, never here.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PushedQuery {
    /// Pushed `WHERE` predicates (`AND`-combined), if the driver supports `where_`.
    pub filter: Option<Predicate>,
    /// Pushed projection columns, if the driver supports `project`.
    pub project: Option<Vec<Name>>,
    /// Pushed `LIMIT`, if the driver supports `limit`.
    pub limit: Option<u64>,
    /// Pushed `ORDER BY` keys, if the driver supports `order`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<PushedOrder>,
    /// Pushed `GROUP BY` columns, if the driver supports `group_by`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<Name>,
    /// Pushed aggregate terms, if the driver supports `aggregate`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aggregates: Vec<PushedAggregate>,
    /// Whether `DISTINCT` was pushed (driver supports `distinct`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub distinct: bool,
}

/// A pushed `ORDER BY` key (serializable mirror of [`OrderKey`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PushedOrder {
    /// The sort column.
    pub column: Name,
    /// `true` for `DESC`.
    pub descending: bool,
}

impl From<&OrderKey> for PushedOrder {
    fn from(k: &OrderKey) -> Self {
        Self {
            column: k.column.clone(),
            descending: k.descending,
        }
    }
}

/// A pushed aggregate term (serializable mirror of [`Aggregate`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PushedAggregate {
    /// The aggregation function label (`count`/`sum`/`min`/`max`).
    pub func: &'static str,
    /// The input column.
    pub column: Name,
    /// The output column.
    pub output: Name,
}

impl From<&Aggregate> for PushedAggregate {
    fn from(a: &Aggregate) -> Self {
        Self {
            func: a.func.label(),
            column: a.column.clone(),
            output: a.output.clone(),
        }
    }
}

impl PushedQuery {
    /// Whether nothing at all was pushed (every op stayed in the residual). A `None`
    /// pushdown profile produces exactly this — a bare scan with no native work.
    #[must_use]
    pub fn is_bare(&self) -> bool {
        self.filter.is_none()
            && self.project.is_none()
            && self.limit.is_none()
            && self.order.is_empty()
            && self.group_by.is_empty()
            && self.aggregates.is_empty()
            && !self.distinct
    }
}

/// A fully-pushed-down subtree: the native work one source runs itself (RFD §6). Carries
/// the [`SourceId`] so the runtime (T10) attributes per-leg timeouts/retries/logs, and
/// the resolved output [`Schema`] so the local combine engine types the residual.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanNode {
    /// The source/driver that executes this scan.
    pub source: SourceId,
    /// The owned description of the pushed work.
    pub pushed: PushedQuery,
    /// The scan's output schema (after the pushed projection/aggregate).
    pub schema: Schema,
}

/// A local residual combine operator — the work the engine runs **after** the native
/// scans return (RFD §6). One variant per residual op the cross-source evaluator must
/// support; `cfs-engine`'s `CombineEngine` executes these.
#[derive(Debug, Clone, PartialEq)]
pub enum CombineOp {
    /// A residual `WHERE` the source could not push (e.g. a `None`-pushdown source, or a
    /// predicate referencing a federated join's columns).
    Filter(Predicate),
    /// A residual projection.
    Project(Vec<Name>),
    /// A residual `LIMIT`.
    Limit(u64),
    /// A residual `ORDER BY`.
    Sort(Vec<OrderKey>),
    /// A residual `DISTINCT`.
    Distinct,
    /// A residual `GROUP BY` + aggregate.
    Aggregate {
        /// The grouping columns.
        group_by: Vec<Name>,
        /// The aggregate terms.
        aggregates: Vec<Aggregate>,
    },
    /// A residual `EXPAND`.
    Expand(Name),
    /// A **federated** hash join over two sub-plans on `on` (cross-source JOIN, RFD §6).
    HashJoin(JoinOn),
    /// A federated set op (`UNION`/`EXCEPT`/`INTERSECT`).
    SetOp(SetKind),
}

impl CombineOp {
    /// A stable label for `explain()` golden output.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            CombineOp::Filter(_) => "Filter",
            CombineOp::Project(_) => "Project",
            CombineOp::Limit(_) => "Limit",
            CombineOp::Sort(_) => "Sort",
            CombineOp::Distinct => "Distinct",
            CombineOp::Aggregate { .. } => "Aggregate",
            CombineOp::Expand(_) => "Expand",
            CombineOp::HashJoin(_) => "HashJoin",
            CombineOp::SetOp(k) => k.label(),
        }
    }
}

/// The physical plan (RFD §6): either a single fully-pushed-down [`ScanNode`], or a
/// local [`CombineOp`] over one or more sub-plans (the residual the engine runs).
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    /// Fully pushed to one source.
    Scan(ScanNode),
    /// A local residual combine over its inputs.
    Combine {
        /// The combine operator.
        op: CombineOp,
        /// The inputs (one for unary ops, two for join/set ops).
        inputs: Vec<PhysicalPlan>,
    },
}

impl PhysicalPlan {
    /// Wrap this plan in a unary local combine op.
    #[must_use]
    pub(crate) fn combine1(op: CombineOp, input: PhysicalPlan) -> Self {
        PhysicalPlan::Combine {
            op,
            inputs: vec![input],
        }
    }

    /// Wrap two plans in a binary local combine op.
    #[must_use]
    pub(crate) fn combine2(op: CombineOp, lhs: PhysicalPlan, rhs: PhysicalPlan) -> Self {
        PhysicalPlan::Combine {
            op,
            inputs: vec![lhs, rhs],
        }
    }

    /// The number of native [`ScanNode`]s in this physical plan — the count T10's
    /// batcher will parallelize. A single-source pipeline has exactly one.
    #[must_use]
    pub fn scan_count(&self) -> usize {
        match self {
            PhysicalPlan::Scan(_) => 1,
            PhysicalPlan::Combine { inputs, .. } => inputs.iter().map(Self::scan_count).sum(),
        }
    }

    /// Every [`ScanNode`] in left-to-right order — the independent native scans T10
    /// surfaces for batching/parallel execution.
    #[must_use]
    pub fn scans(&self) -> Vec<&ScanNode> {
        let mut out = Vec::new();
        self.collect_scans(&mut out);
        out
    }

    fn collect_scans<'a>(&'a self, out: &mut Vec<&'a ScanNode>) {
        match self {
            PhysicalPlan::Scan(s) => out.push(s),
            PhysicalPlan::Combine { inputs, .. } => {
                for i in inputs {
                    i.collect_scans(out);
                }
            }
        }
    }
}
