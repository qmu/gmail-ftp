//! `cfs-engine` — the **local combine engine** (RFD-0001 §6, ticket t14, ADR-0002).
//!
//! The pushdown planner (`cfs-pushdown`) splits a query into native [`ScanNode`]s the
//! drivers run themselves plus a local residual ([`PhysicalPlan::Combine`] ops). This
//! crate is the seam that **executes that residual** over the scan results: filter,
//! project, hash-join, set ops, group/aggregate, sort, limit, EXPAND.
//!
//! ## The engine decision (ADR-0002): own [`MiniEvaluator`], not embedded DuckDB
//! The DuckDB-vs-own question RFD §6 flags is resolved in `docs/adr/0002-local-combine-engine.md`:
//! we ship an in-house [`MiniEvaluator`] behind the [`CombineEngine`] trait. DuckDB
//! cannot build to `wasm32-unknown-unknown` (the Cloudflare Workers target, RFD §1/§9)
//! and adds a large static C++ footprint, contradicting the "single binary / wasm32 / no
//! heavy vendor SDK" rule — while the residual is only ever a **small** relational subset
//! (the heavy lifting is pushed down). The trait keeps the choice reversible: an optional
//! `DuckDbEngine` could land behind a non-default feature without touching callers.
//!
//! ## Correctness contract (the differential property)
//! [`MiniEvaluator::execute`] over a [`PhysicalPlan`] returns exactly the rows a naive
//! all-local evaluation of the original query would (`cfs-pushdown` guarantees the split
//! is semantically total; this crate's tests assert push-then-combine == all-local).
//!
//! ## Purity / wasm-friendliness
//! Dependency-light (only `cfs-pushdown` + `cfs-types`): no I/O, no async, no threads, no
//! vendor SDK. The engine computes over owned [`RowBatch`]es a (future) driver produced;
//! it never opens a socket itself.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod combine;
mod eval;
mod scan;

pub use combine::{CombineEngine, EngineError, MiniEvaluator};
pub use scan::ScanResults;
