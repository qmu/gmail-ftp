//! `cfs-types` — the row/relation **type & schema model** (RFD-0001 §4 data &
//! type model; the schema half of §5 "driver declares schema, powers `DESCRIBE`").
//!
//! This is the canonical static description of a relation ([`Schema`] of typed,
//! possibly-nested [`Column`]s) and the runtime data that flows through a pipeline
//! ([`Value`] / [`Row`] / [`RowBatch`]). Everything downstream computes over it:
//! `EXPAND <field>` explodes a `Struct`/`Array` column ([`Schema::expand`]); path
//! access `a.b.c` navigates `Struct`s without flattening ([`Schema::resolve_path`]);
//! typed predicates need column types to type-check and drive pushdown
//! ([`typecheck_predicate`]); codecs (`DECODE`/`ENCODE`) bridge `bytes ↔ rows` and so
//! target [`Row`]/[`RowBatch`]; `UNION` over heterogeneous sources widens column-wise
//! ([`Schema::unify`]).
//!
//! ## Purity & determinism (RFD §3 purity invariant)
//! Every function here is **pure and total** over its inputs — no I/O, no clock, no
//! RNG. That is what lets `PREVIEW` / type-check run with **no live creds** and be
//! golden-tested deterministically. The one effectful seam ([`SchemaSource`]) is a
//! *trait surface only*; real impls live in E4 drivers.
//!
//! ## Owned DTOs / no vendor leak (RFD §9, boundary B3)
//! Nothing here depends on a driver SDK or any vendor type. Drivers map their catalog
//! into [`Schema`] at the boundary; the engine never sees a vendor type. The crate is
//! **dependency-free beyond `serde`** and is a true leaf of the workspace spine.
//!
//! ## Capability / least-privilege (RFD §10)
//! Types carry **no secrets** and no capability state. [`Provenance`] records a
//! *driver id only* ([`DriverId`]), never a token — nothing here can leak a credential.
//!
//! ## wasm-friendliness (boundary guard B7)
//! No threads, no `std::fs`, no sockets, no `unsafe`. Pure data + pure functions, so
//! the crate compiles for `wasm32-unknown-unknown` (acceptance criterion).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod error;
mod predicate;
mod schema;
mod unify;
mod value;

pub use error::TypeError;
pub use predicate::{
    typecheck_predicate, CmpOp, ColRef, Literal, Pattern, Predicate, TypedPredicate,
};
pub use schema::{Column, ColumnType, DriverId, Name, Provenance, Schema, SchemaSource, Typed};
pub use value::{Row, RowBatch, Value};
