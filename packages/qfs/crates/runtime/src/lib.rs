//! `qfs-runtime` — the effect-plan **interpreter** (RFD-0001 §3 `COMMIT : Plan -> World`,
//! §6 runtime). This is the **sole impure stage** of qfs: everything upstream
//! (parse → resolve → evaluate, E1) constructs effects as pure data ([`qfs_plan::Plan`]);
//! this crate is the one place that touches the World.
//!
//! ## What it does (RFD §6 — Haxl-style auto-batching with parallelism)
//! [`Interpreter::commit`] sees the *whole* effect DAG before running anything, so it can:
//! - **Walk in topological frontiers** ([`schedule::Frontier`]) honoring dependency edges.
//! - **Auto-batch** ([`batch::coalesce`]) every independent same-`(driver, kind)` effect at
//!   a frontier into **one** [`ApplyDriver::apply_batch`] call — collapsing the Gmail N+1
//!   message-fetch into a single batched dispatch (the N+1 → 1 property).
//! - **Run independent groups in parallel** under two-level concurrency caps
//!   ([`ConcurrencyLimits`]: a global ceiling + a per-driver ceiling via `tokio` semaphores),
//!   so a wide frontier cannot exhaust file descriptors, rate limits, or memory (backpressure).
//! - **Per-leg timeout + bounded retry** ([`RetryPolicy`]) — but **never** retry an
//!   `irreversible` leg (`REMOVE`, `CALL mail.send`), per RFD §6 idempotency.
//! - **Skip a failed node's transitive dependents** (the t09 `commit` semantics), preserved
//!   under parallelism by an incremental taint-propagating frontier.
//! - **Re-check capability gating** at apply time ([`CapabilitySet`]) — defense in depth over
//!   the parse-time gate (t13).
//! - Thread the **applied-effect ledger** ([`Outcome`] / [`LedgerEntry`]) — the recovery
//!   substrate + audit trail (RFD §6/§10), recording metadata only, never payloads or tokens.
//!
//! [`Interpreter::preview`] is the dry run (RFD §6/§7): it walks the same DAG and produces
//! the same ledger shape **without calling any driver** — no I/O, no side effects.
//!
//! ## Why a separate crate (purity invariant, RFD §3)
//! The interpreter uses `async`/`tokio` for parallelism. `qfs-plan` (the effect substrate)
//! must stay I/O-free — its purity dep-closure test forbids `tokio`. So the interpreter lives
//! here, depending on `qfs-plan` (effect types) + `qfs-types` (the owned `DriverId`) **only**;
//! it has **no** `qfs-core` dependency (avoiding a runtime → core inversion; Architect t07
//! guidance) and walks `qfs-plan` types exclusively.
//!
//! ## READ / source nodes (t08 carry-over)
//! The closed [`qfs_plan::EffectKind`] already includes `Read`/`List` as first-class effect
//! nodes (pure data-acquisition dependencies of a downstream write). The interpreter executes
//! those exactly like any other effect — dispatched to the target driver's
//! [`ApplyDriver::apply_batch`] under `Read`/`List` — so a `READ` leaf that feeds an
//! `UPDATE … FROM <read>` is just an upstream frontier node. The stdlib's *separate*
//! `qfs_core::PlanNode` DTO (the `READ`/`http.get` table-valued source produced inside an
//! expression) is a **core-local** representation; **lifting it into a `qfs_plan::EffectNode`
//! is the evaluator's job** (E1/t14), not the runtime's — the runtime deliberately does not
//! depend on `qfs-core`. See `event-log`/`plan.md` for the recorded boundary decision.
//!
//! ## Driver boundary (RFD §9 — no vendor leak)
//! The runtime resolves effects through a [`DriverRegistry`] of `Arc<dyn ApplyDriver>` and
//! keys batching on owned [`qfs_types::DriverId`] + [`qfs_plan::EffectKind`] — **never** a
//! vendor SDK type. A real E4 driver bridges its synchronous `qfs_plan::PlanApplier` (t09) to
//! the async [`ApplyDriver`] with a thin adapter; tests use an in-memory mock with **no live
//! credentials and no network**.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod batch;
mod bridge;
mod caps;
mod driver;
mod error;
mod interpreter;
mod observe;
mod outcome;
mod schedule;
mod txn;

pub use batch::{coalesce, BatchGroup, GroupKey};
pub use bridge::{PlanApplierBridge, SharedApplier};
pub use caps::{CapabilitySet, ConcurrencyLimits, RetryPolicy};
pub use driver::{ApplyCx, ApplyDriver, DriverRegistry, EffectInput};
pub use error::{ApplyError, EffectError};
pub use interpreter::Interpreter;
pub use observe::TraceId;
pub use outcome::{EffectOutput, LedgerEntry, LegStatus, Outcome};
pub use schedule::{Frontier, Ready};
pub use txn::Preconditions;
// Re-export the transactional envelope (t11) so a runtime consumer (E1/E4/server) drives
// `Interpreter::commit_txn` and reads the recovery report without importing `qfs-txn`
// directly. The pure orchestration types are defined in `qfs-txn`.
pub use qfs_txn::{
    select_strategy, AuditLedger, CommitStrategy, EffectDescriptor, EffectKey, EffectLeg,
    EffectReceipt, Etag, InMemoryLedger, LegOutcome, Precondition, RecoveryReport,
    TransactionalDrivers, Version,
};
