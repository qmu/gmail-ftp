//! `qfs-sql-core` — the **pure-leaf SQL compile/emit core** (RFD-0001 §5/§6, extracted from t17
//! for the t23 Cloudflare D1 reuse). It owns the dialect-agnostic, **injection-safe**
//! qfs-query → parameterized-SQL machinery and the owned catalog DTOs, with **no** runtime,
//! secrets, or driver coupling — its only workspace dependency is `qfs-types`.
//!
//! ## Why this crate exists — single-source the sqlite emitter (the t23 reuse)
//! D1 is SQLite-over-HTTP, so the Cloudflare driver (t23) reuses the **same** `Dialect::Sqlite`
//! emitter and pure query compiler the SQL driver (t17) uses. Before this crate that logic lived
//! inside `qfs-driver-sql`, but that crate is a **runtime leaf** (it depends on `qfs-runtime` for
//! its applier bridge), and the dependency-direction invariant forbids one runtime leaf from
//! depending on another (tokio must dead-end in each leaf). Extracting the pure core here — the
//! same single-source pattern as `qfs-http-core` — lets BOTH driver crates reuse one emitter
//! while each stays an independent runtime leaf.
//!
//! ## Injection safety (the headline invariant, preserved)
//! [`emit`] binds **every** value as a parameter (`$n`/`?`); the SQL string carries only quoted
//! identifiers and placeholders. A value like `'; DROP TABLE t; --` is bound as data, never
//! executed — and an HTTP backend (D1) ships `params` as a structured bound array, never
//! interpolated.
//!
//! ## Purity
//! Every function here is pure (no I/O, no clock, no RNG, no runtime). The runtime/secrets
//! adapters for [`SqlError`] live in the consuming driver crates, not here.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod catalog;
pub mod compile;
mod dialect;
pub mod emit;
mod error;

pub use catalog::{Catalog, ColumnDef, RelationKind, TableCatalog};
pub use compile::{compile, CompileResult, QuerySpec};
pub use dialect::Dialect;
pub use emit::{render_dml, render_select, DmlOp, OrderTerm, Param, SelectPlan, SqlPredicate};
pub use error::SqlError;
