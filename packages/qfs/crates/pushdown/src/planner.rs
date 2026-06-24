//! [`partition_by_source`] — the source-splitting pass (RFD §6, ticket t14 step 2/3).
//!
//! Post-order walk of the [`LogicalPlan`]:
//!
//! - A **linear unary chain over a single scan** (the common `FROM /x |> WHERE |> SELECT
//!   |> LIMIT` shape) is a pushdown candidate: the planner offers each stage to the
//!   source's [`PushdownProfile`](qfs_driver::PushdownProfile) (queried by intent via the
//!   `supports_*` accessors, t13), pushing what it accepts into a [`PushedQuery`] and
//!   re-grafting the residual as local [`CombineOp`]s.
//! - A `JOIN`/`UNION`/`EXCEPT`/`INTERSECT` **always federates locally**: each side is
//!   partitioned independently (and maximally pushed to its own source) and combined by
//!   the matching local combine op. A cross-source join therefore runs locally over each
//!   side's pushed-down result (RFD §6 federation). Native join pushdown into a single
//!   source is an explicit E4 refinement, out of t14 scope.
//!
//! The split is **semantically total**: the residual local plan over the scans returns
//! exactly the rows a naive all-local evaluation would (the differential property the
//! `qfs-engine` tests assert). Capability/policy denial fails at plan time, never as a
//! partial scan.

use std::collections::BTreeMap;

use qfs_driver::PushdownProfile;
use qfs_types::{Column, ColumnType, Name, Predicate, Schema};

use crate::error::PlanError;
use crate::logical::{Aggregate, Aggregator, LogicalPlan, SourceId};
use crate::physical::{CombineOp, PhysicalPlan, PushedOrder, PushedQuery, ScanNode};

/// The pushdown input for the planner: which sources exist and what each can run
/// natively. A thin map `SourceId → PushdownProfile`, populated from the driver registry
/// (each driver's [`Driver::pushdown`](qfs_driver::Driver::pushdown) keyed by its
/// [`Driver::id`](qfs_driver::Driver::id)). Kept here (not a `DriverRegistry` directly)
/// so this Domain crate stays free of the registry's `Arc<dyn Driver>` machinery and
/// I/O-free; the caller adapts the registry into this at the boundary.
#[derive(Debug, Default)]
pub struct SourceRegistry {
    entries: BTreeMap<SourceId, SourceEntry>,
}

/// One registered source: its pushdown profile plus whether it admits a read at all
/// (the verb-capability gate, RFD §5). A source that cannot `SELECT` is rejected at plan
/// time rather than emitting a scan that would fail at execution.
#[derive(Debug, Clone, Copy)]
struct SourceEntry {
    profile: PushdownProfile,
    readable: bool,
}

impl SourceRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a readable source's pushdown profile (overwrites any prior entry).
    #[must_use]
    pub fn with(mut self, source: SourceId, profile: PushdownProfile) -> Self {
        self.register(source, profile);
        self
    }

    /// Register a readable source's pushdown profile in place.
    pub fn register(&mut self, source: SourceId, profile: PushdownProfile) {
        self.entries.insert(
            source,
            SourceEntry {
                profile,
                readable: true,
            },
        );
    }

    /// Register a source that **cannot** be read (its node denies `SELECT`). A scan over
    /// it fails at plan time with [`PlanError::CapabilityDenied`] (RFD §5/§10).
    pub fn register_unreadable(&mut self, source: SourceId, profile: PushdownProfile) {
        self.entries.insert(
            source,
            SourceEntry {
                profile,
                readable: false,
            },
        );
    }

    /// The profile for a source, if registered.
    #[must_use]
    pub fn profile(&self, source: &SourceId) -> Option<&PushdownProfile> {
        self.entries.get(source).map(|e| &e.profile)
    }

    /// Whether the source admits a read (defaults to "unknown source" handling when
    /// absent — the caller resolves `None` to [`PlanError::UnknownSource`]).
    fn readable(&self, source: &SourceId) -> Option<bool> {
        self.entries.get(source).map(|e| e.readable)
    }
}

/// Partition a [`LogicalPlan`] into a [`PhysicalPlan`] of native [`ScanNode`]s + a local
/// [`CombineOp`] residual (RFD §6). Pushes maximal work to each source; federates the
/// rest locally.
///
/// # Errors
/// [`PlanError::UnknownSource`] for an unregistered source; [`PlanError::CapabilityDenied`]
/// if a source's profile cannot run the base scan it is asked for (so the planner rejects
/// at plan time rather than emitting an unrunnable scan).
pub fn partition_by_source(
    plan: &LogicalPlan,
    reg: &SourceRegistry,
) -> Result<PhysicalPlan, PlanError> {
    // A linear unary chain over a single scan is a pushdown candidate. JOIN/SetOp nodes
    // (even single-source ones) federate locally for deterministic, rule-based plans.
    if let Some(source) = single_source_chain(plan) {
        return push_chain(plan, &source, reg);
    }
    federate(plan, reg)
}

/// `Some(source)` iff `plan` is a **linear unary chain** (`Filter`/`Project`/`Limit`/
/// `Sort`/`Distinct`/`Aggregate`/`Expand`) bottoming out in a single [`Scan`], all from
/// one source. A JOIN/SetOp anywhere makes this `None` (it federates instead).
fn single_source_chain(plan: &LogicalPlan) -> Option<SourceId> {
    match plan {
        LogicalPlan::Scan { source, .. } => Some(source.clone()),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Expand { input, .. } => single_source_chain(input),
        LogicalPlan::Join { .. } | LogicalPlan::SetOp { .. } => None,
    }
}

/// Push a linear single-source chain to its source, accumulating the accepted work in a
/// [`PushedQuery`] and re-grafting the residual as local combine ops.
fn push_chain(
    plan: &LogicalPlan,
    source: &SourceId,
    reg: &SourceRegistry,
) -> Result<PhysicalPlan, PlanError> {
    let profile = reg
        .profile(source)
        .ok_or_else(|| PlanError::unknown_source(source))?;
    // Capability gate (RFD §5/§10): a source whose node denies SELECT is rejected at
    // plan time — never a partial scan.
    if reg.readable(source) == Some(false) {
        return Err(PlanError::capability_denied(source, "SELECT"));
    }

    let mut acc = Acc::new(source.clone(), scan_path(plan), profile);
    let schema = walk_chain(plan, &mut acc);
    Ok(acc.finish(schema))
}

/// The concrete addressed VFS path of the single `Scan` leaf at the bottom of a unary chain
/// (t28). Empty if the chain bottoms out in a non-path source.
fn scan_path(plan: &LogicalPlan) -> String {
    match plan {
        LogicalPlan::Scan { path, .. } => path.clone(),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Expand { input, .. } => scan_path(input),
        LogicalPlan::Join { .. } | LogicalPlan::SetOp { .. } => String::new(),
    }
}

/// The accumulator threaded down a single-source chain: the growing [`PushedQuery`] plus
/// the residual local ops (built innermost-first, applied outermost-last).
struct Acc<'p> {
    source: SourceId,
    /// The concrete addressed VFS path of the scan leaf (t28).
    path: String,
    profile: &'p PushdownProfile,
    pushed: PushedQuery,
    /// Residual ops, innermost first. Once a stage is forced local, every later
    /// (outer) stage must also stay local — you cannot push a stage *below* a local op —
    /// so `local_pinned()` stops pushing.
    residual: Vec<CombineOp>,
}

impl<'p> Acc<'p> {
    fn new(source: SourceId, path: String, profile: &'p PushdownProfile) -> Self {
        Self {
            source,
            path,
            profile,
            pushed: PushedQuery::default(),
            residual: Vec::new(),
        }
    }

    fn local_pinned(&self) -> bool {
        !self.residual.is_empty()
    }

    fn force_local(&mut self, op: CombineOp) {
        self.residual.push(op);
    }

    /// Assemble the final physical plan: the native scan wrapped by the residual ops,
    /// innermost residual first (outermost last).
    fn finish(self, schema: Schema) -> PhysicalPlan {
        let mut node = PhysicalPlan::Scan(ScanNode {
            source: self.source,
            path: self.path,
            pushed: self.pushed,
            schema,
        });
        for op in self.residual {
            node = PhysicalPlan::combine1(op, node);
        }
        node
    }
}

/// Walk a single-source chain post-order, pushing each stage when the source supports it
/// (and nothing has yet been forced local), else forcing it local. Returns the chain's
/// resolved output schema (for the scan node).
fn walk_chain(plan: &LogicalPlan, acc: &mut Acc) -> Schema {
    match plan {
        LogicalPlan::Scan { schema, .. } => schema.clone(),
        LogicalPlan::Filter { input, predicate } => {
            let schema = walk_chain(input, acc);
            if !acc.local_pinned() && acc.profile.supports_where() {
                acc.pushed.filter =
                    Some(and_predicate(acc.pushed.filter.take(), predicate.clone()));
            } else {
                acc.force_local(CombineOp::Filter(predicate.clone()));
            }
            schema
        }
        LogicalPlan::Project { input, columns } => {
            let schema = walk_chain(input, acc);
            let out = project_schema(&schema, columns);
            if !acc.local_pinned() && acc.profile.supports_project() {
                acc.pushed.project = Some(columns.clone());
            } else {
                acc.force_local(CombineOp::Project(columns.clone()));
            }
            out
        }
        LogicalPlan::Limit { input, n } => {
            let schema = walk_chain(input, acc);
            if !acc.local_pinned() && acc.profile.supports_limit() {
                acc.pushed.limit = Some(*n);
            } else {
                acc.force_local(CombineOp::Limit(*n));
            }
            schema
        }
        LogicalPlan::Sort { input, keys } => {
            let schema = walk_chain(input, acc);
            if !acc.local_pinned() && acc.profile.supports_order() {
                acc.pushed.order = keys.iter().map(PushedOrder::from).collect();
            } else {
                acc.force_local(CombineOp::Sort(keys.clone()));
            }
            schema
        }
        LogicalPlan::Distinct { input } => {
            let schema = walk_chain(input, acc);
            if !acc.local_pinned() && acc.profile.supports_distinct() {
                acc.pushed.distinct = true;
            } else {
                acc.force_local(CombineOp::Distinct);
            }
            schema
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            walk_chain(input, acc);
            let out = aggregate_schema(group_by, aggregates);
            let can_push = !acc.local_pinned()
                && acc.profile.supports_aggregate()
                && (group_by.is_empty() || acc.profile.supports_group_by());
            if can_push {
                acc.pushed.group_by = group_by.clone();
                acc.pushed.aggregates = aggregates.iter().map(Into::into).collect();
            } else {
                acc.force_local(CombineOp::Aggregate {
                    group_by: group_by.clone(),
                    aggregates: aggregates.clone(),
                });
            }
            out
        }
        LogicalPlan::Expand { input, field } => {
            let schema = walk_chain(input, acc);
            // EXPAND is never pushed (no driver declares it); always a local combine.
            acc.force_local(CombineOp::Expand(field.clone()));
            // The expanded field may be late-bound (Unknown); the input schema is a
            // conservative approximation of the residual's output.
            schema
        }
        // `single_source_chain` already excluded JOIN/SetOp, so these never reach here.
        // Returning an empty schema keeps the function total without a panic.
        LogicalPlan::Join { .. } | LogicalPlan::SetOp { .. } => Schema::empty(),
    }
}

/// Federate a cross-source (or join/set) node: recurse into each side via
/// [`partition_by_source`] and wrap them in the matching local [`CombineOp`]. A unary
/// node over a cross-source input keeps its op local above the federated child.
fn federate(plan: &LogicalPlan, reg: &SourceRegistry) -> Result<PhysicalPlan, PlanError> {
    match plan {
        LogicalPlan::Join { lhs, rhs, on, .. } => {
            let l = partition_by_source(lhs, reg)?;
            let r = partition_by_source(rhs, reg)?;
            Ok(PhysicalPlan::combine2(
                CombineOp::HashJoin(on.clone()),
                l,
                r,
            ))
        }
        LogicalPlan::SetOp { kind, lhs, rhs } => {
            let l = partition_by_source(lhs, reg)?;
            let r = partition_by_source(rhs, reg)?;
            Ok(PhysicalPlan::combine2(CombineOp::SetOp(*kind), l, r))
        }
        LogicalPlan::Filter { input, predicate } => Ok(PhysicalPlan::combine1(
            CombineOp::Filter(predicate.clone()),
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Project { input, columns } => Ok(PhysicalPlan::combine1(
            CombineOp::Project(columns.clone()),
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Limit { input, n } => Ok(PhysicalPlan::combine1(
            CombineOp::Limit(*n),
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Sort { input, keys } => Ok(PhysicalPlan::combine1(
            CombineOp::Sort(keys.clone()),
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Distinct { input } => Ok(PhysicalPlan::combine1(
            CombineOp::Distinct,
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Ok(PhysicalPlan::combine1(
            CombineOp::Aggregate {
                group_by: group_by.clone(),
                aggregates: aggregates.clone(),
            },
            partition_by_source(input, reg)?,
        )),
        LogicalPlan::Expand { input, field } => Ok(PhysicalPlan::combine1(
            CombineOp::Expand(field.clone()),
            partition_by_source(input, reg)?,
        )),
        // A bare scan is a single-source chain, routed through `push_chain`, so this is
        // unreachable in practice; handled defensively (no partial scan).
        LogicalPlan::Scan {
            source,
            path,
            schema,
        } => {
            reg.profile(source)
                .ok_or_else(|| PlanError::unknown_source(source))?;
            if reg.readable(source) == Some(false) {
                return Err(PlanError::capability_denied(source, "SELECT"));
            }
            Ok(PhysicalPlan::Scan(ScanNode {
                source: source.clone(),
                path: path.clone(),
                pushed: PushedQuery::default(),
                schema: schema.clone(),
            }))
        }
    }
}

/// AND-combine an existing pushed predicate with a new one (multiple `WHERE`s push as a
/// conjunction).
fn and_predicate(existing: Option<Predicate>, next: Predicate) -> Predicate {
    match existing {
        Some(prev) => Predicate::And(Box::new(prev), Box::new(next)),
        None => next,
    }
}

/// The projected schema for a column list (`*` / `["*"]` / empty is identity).
fn project_schema(input: &Schema, columns: &[Name]) -> Schema {
    if columns.is_empty() || columns == ["*".to_string()] {
        return input.clone();
    }
    let cols = columns
        .iter()
        .filter_map(|name| input.column(name).cloned())
        .collect();
    Schema::new(cols)
}

/// The output schema of a `GROUP BY` + aggregate (group columns then one column per
/// aggregate term, named by its `output`).
fn aggregate_schema(group_by: &[Name], aggregates: &[Aggregate]) -> Schema {
    let mut cols = Vec::with_capacity(group_by.len() + aggregates.len());
    for g in group_by {
        cols.push(Column::new(g.clone(), ColumnType::Unknown, true));
    }
    for a in aggregates {
        let ty = match a.func {
            Aggregator::Count => ColumnType::Int,
            _ => ColumnType::Unknown,
        };
        cols.push(Column::new(a.output.clone(), ty, true));
    }
    Schema::new(cols)
}
