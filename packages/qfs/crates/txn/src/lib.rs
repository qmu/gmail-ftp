//! `qfs-txn` — the transactional **correctness envelope** (RFD-0001 §6 runtime, §10 safety).
//!
//! The t10 interpreter walks the effect-plan DAG, batches, and parallelizes. This crate
//! gives that interpreter its correctness guarantees, as **pure orchestration** over the
//! effect plan (no `async`, no I/O, no vendor SDK types — the impure apply is reached only
//! through the synchronous [`LegApplier`] seam the runtime adapts its async driver to):
//!
//! - **Idempotency.** A deterministic [`EffectKey`] (content hash of `(plan, node, target,
//!   kind, args)`, stable across retries and batch reordering) plus the append-before-apply
//!   [`AuditLedger`] make a retried / re-delivered effect a no-op
//!   ([`LegOutcome::AlreadyApplied`]). `UPSERT` (modelled distinctly from `INSERT` in
//!   `qfs-plan`) is the driver-side dedup point; the ledger covers procs that are not
//!   naturally idempotent. Irreversible legs are applied **at most once** (never retried).
//! - **Optimistic concurrency.** A [`Precondition`] (`If-Version` / `If-Match` ETag) is
//!   captured at the read and travels **on the effect node**, so the t10 batch/parallel
//!   reorder cannot lose it. A stale version yields a typed [`LegOutcome::Conflict`]; the
//!   saga bounded-auto-retries (re-read → re-apply) before surfacing it — no lost update.
//! - **Transactions.** [`select_strategy`] inspects the plan's write targets: a single
//!   transactional source runs as one ACID `BEGIN…COMMIT`/rollback ([`SagaExecutor::run_acid`]);
//!   a plan spanning sources becomes an orchestrated best-effort saga
//!   ([`SagaExecutor::run_saga`]) with reverse-order [`Compensation`] and ledger recovery.
//! - **Recoverable `cp`/`mv`.** A cross-mount move compiles to the [`CpStep`] triple
//!   copy → verify → delete (never delete-before-verify), so a crash leaves a harmless
//!   duplicate, never a hole.
//!
//! Every commit emits a secret-free [`RecoveryReport`]; the [`AuditLedger`] is the
//! audit-of-record. The durable ledger sink and observability plumbing are deferred to E8 —
//! [`AuditLedger`] is the only seam, so swapping the backend is trivial.
//!
//! ## Spine (acyclic): `qfs-txn → { qfs-plan, qfs-types }`
//! Pure data + orchestration only. `tokio`/`async` stay confined to `qfs-runtime`, which
//! depends on `qfs-txn` and bridges its async `ApplyDriver` to the synchronous [`LegApplier`].

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod key;
mod ledger;
mod leg;
mod outcome;
mod report;
mod saga;
mod strategy;
mod version;

pub use key::EffectKey;
pub use ledger::{AuditLedger, InMemoryLedger};
pub use leg::{Compensation, EffectLeg, LegApplier};
pub use outcome::{EffectDescriptor, EffectError, EffectReceipt, LegOutcome};
pub use report::{LegRecord, RecoveryReport};
pub use saga::{all_succeeded, SagaExecutor, SagaPolicy};
pub use strategy::{select_strategy, CommitStrategy, CpStep, TransactionalDrivers};
pub use version::{Etag, Precondition, Version};

#[cfg(test)]
mod tests;
