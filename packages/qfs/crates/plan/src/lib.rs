//! `qfs-plan` — the effect substrate (RFD-0001 §3 purity invariant, §6 runtime).
//!
//! qfs's central safety property is **effects-as-data**: write operators
//! (`cp/mv/INSERT/UPSERT/UPDATE/REMOVE/CALL`) do not execute — they evaluate to a
//! [`Plan`], a typed DAG of [`EffectNode`]s. The only impure operation is the
//! interpreter ([`commit`], `Plan -> World -> World`), expressed here as the
//! [`PlanApplier`] seam; constructing a plan and previewing it perform **no I/O**.
//!
//! ## Purity invariant (load-bearing, RFD §3)
//! This crate is free of `async`, I/O, and vendor SDK types. The compiler makes
//! "constructing a plan does I/O" unrepresentable: every combinator
//! ([`Plan::leaf`]/[`Plan::pure`]/[`Plan::then`]/[`Plan::merge`]) returns a new pure
//! [`Plan`]; the single impure seam is [`PlanApplier::apply`], called only by
//! [`commit`]. `CALL driver.x` builds an [`EffectKind::Call`] node — it never performs
//! the call.
//!
//! ## PREVIEW vs COMMIT (RFD §6/§7)
//! - **PREVIEW** ([`preview`]) is a dry run: it reads the plan and returns a
//!   deterministic, **secret-free** [`Preview`] (tree + per-node affected counts +
//!   irreversible warnings), applying nothing. It has `Display` (human) and
//!   `Serialize` (`-json`).
//! - **COMMIT** ([`commit`]) walks the DAG in topological order calling a
//!   [`PlanApplier`], the only place side effects (and secrets) occur, and returns a
//!   [`CommitReport`] (applied / skipped-due-to-failed-dependency).
//!
//! ## Least-privilege / no vendor leak (RFD §9/§10)
//! A [`Plan`] carries [`DriverId`] + [`VfsPath`] + owned [`qfs_types::RowBatch`] data
//! only — never credentials or tokens, so previews are safe to log. Secrets enter at
//! the [`PlanApplier`] boundary (E4), keeping the audit ledger and POLICY gating
//! attached to the applier, not the plan.
//!
//! ## Evaluator wiring (E1)
//! The evaluator that consumes the `qfs-parser` AST and emits a [`Plan`] lives in
//! `qfs-core` (the reserved `qfs-core → qfs-parser` edge, acceptance criterion C5),
//! not here, so that this crate keeps **zero** dependency on the parser and stays a
//! low, pure node of the spine. [`kind_for_verb`] gives that evaluator the canonical
//! AST-verb → [`EffectKind`] mapping without `qfs-plan` depending on the AST type.
//!
//! ## wasm-friendliness (boundary guard B7)
//! Pure data: no threads, no `std::fs`, no sockets.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod apply;
mod ids;
mod node;
mod plan;
mod preview;
mod server;
mod topo;

pub use apply::{
    commit, AppliedEffect, ApplyError, CommitReport, PlanApplier, RecordingApplier, SkipReason,
};
pub use ids::{Affected, DriverId, NodeId, ProcId, Target, VfsPath};
pub use node::{EffectKind, EffectNode};
pub use plan::{depends_on, Plan, PlanBuilder, PlanError};
pub use preview::{preview, Preview, PreviewRow};
pub use server::{ServerNode, ServerWriteOp};
pub use topo::topo_order;

/// The four AST effect verbs, mirrored here so the E1 evaluator can translate
/// `qfs_parser::EffectVerb` into an [`EffectKind`] without this crate depending on the
/// parser (keeping the spine acyclic and `qfs-plan` parser-free). The evaluator maps
/// `qfs_parser::EffectVerb::{Insert,Upsert,Update,Remove}` onto these identically-named
/// variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteVerb {
    /// `INSERT INTO`
    Insert,
    /// `UPSERT INTO`
    Upsert,
    /// `UPDATE`
    Update,
    /// `REMOVE`
    Remove,
}

/// Map an AST write verb to its [`EffectKind`]. The single source of truth for the
/// evaluator's verb translation; `Remove` is inherently irreversible (RFD §10) and
/// that is reflected when the node is built via [`EffectNode::new`].
#[must_use]
pub fn kind_for_verb(verb: WriteVerb) -> EffectKind {
    match verb {
        WriteVerb::Insert => EffectKind::Insert,
        WriteVerb::Upsert => EffectKind::Upsert,
        WriteVerb::Update => EffectKind::Update,
        WriteVerb::Remove => EffectKind::Remove,
    }
}

#[cfg(test)]
mod tests;
