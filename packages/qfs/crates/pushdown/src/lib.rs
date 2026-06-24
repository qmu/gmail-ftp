//! `qfs-pushdown` — the **pushdown planner** (RFD-0001 §6 pushdown federation, the
//! federation half of epic E3, ticket t14).
//!
//! A qfs pipeline (`FROM <path> |> <op> |> …`) may straddle several sources. This crate
//! is the pure pass that:
//!
//! 1. lowers a query into a [`LogicalPlan`] (pure-query operators only — no effect
//!    variants), built from the AST upstream;
//! 2. tags each leaf with its [`SourceId`] and finds each **maximal same-source
//!    subtree**;
//! 3. negotiates that subtree against the driver's [`PushdownProfile`](qfs_driver::PushdownProfile)
//!    (queried by intent via the `supports_*` accessors, t13) — emitting a native
//!    [`ScanNode`] (an opaque, owned [`PushedQuery`] the driver later translates to SQL/
//!    plumbing) for the accepted part and a local [`PhysicalPlan::Combine`] residual for
//!    the rest; and
//! 4. renders a deterministic [`explain`] plan-dump for golden tests.
//!
//! A `JOIN`/`UNION`/`EXCEPT`/`INTERSECT` across two **different** `SourceId`s always
//! becomes a local `Combine` over each side's pushed-down result (federation).
//!
//! ## Predicates are sourced from the typed model, not a dropped AST (O-t07-3)
//! The t07 evaluator's `PlanSource` is a *schema-threading* IR whose `Filter`/`Project`/
//! `Join` deliberately drop the predicate/expression/`on` ASTs. This crate does **not**
//! consume `PlanSource`; per the t07 carry-over O-t07-1 it builds its **own**
//! [`LogicalPlan`] from the AST. Its [`LogicalPlan::Filter`] carries a
//! [`qfs_types::Predicate`] (the typed predicate IR, t05) so a `WHERE` survives into the
//! planner and can be split source-by-source. The lowering from the parser `Expr` to the
//! typed `Predicate` lives in [`lower`].
//!
//! ## Purity & determinism (RFD §3)
//! No function here performs I/O, takes a `&mut self`, or touches a clock/RNG. Rule-based
//! partitioning only: identical input ⇒ byte-identical [`explain`] output, so golden
//! tests are stable and AI agents get reproducible plans. The pushdown *declaration* is
//! the driver's; the *split* is this crate's; the *execution* of the residual is
//! `qfs-engine`'s.
//!
//! ## No vendor leak (RFD §9)
//! [`PushedQuery`] is an owned engine-side description (predicates, projection, limit,
//! …) — never a vendor query object; the driver translates it inside its own boundary.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod error;
mod explain;
mod logical;
mod lower;
mod physical;
mod planner;

pub use error::PlanError;
pub use explain::explain;
pub use logical::{
    Aggregate, Aggregator, JoinKind, JoinOn, LogicalPlan, OrderKey, SetKind, SourceId,
};
pub use lower::{lower_predicate, lower_query, LowerError};
pub use physical::{CombineOp, PhysicalPlan, PushedQuery, ScanNode};
pub use planner::{partition_by_source, SourceRegistry};
