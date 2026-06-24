//! `qfs-cron` — the JOB scheduler / cron binding (t33, RFD-0001 §8).
//!
//! Makes `CREATE JOB <name> EVERY <interval> DO <plan>` bindings (stored in `/server/jobs` by t31)
//! fire on cadence. A **thin runtime**: it constructs no effects and performs no service I/O — it
//! only *causes* an already-built `Plan` to commit through an INJECTED [`Committer`] (the purity
//! invariant, RFD §3/§6). The scheduler stays pure; the REAL commit path (the runtime
//! `Interpreter`/applier with live drivers) is provided by the `qfs` binary's serve composition
//! root, which also wires the `/server`-backed `JobStore`.
//!
//! ## Pure core vs native daemon (the t25 optional-runtime split)
//! The PURE scheduler core — [`Schedule`] math + restricted-5-field cron, [`MissedPolicy`]
//! due-set folding, the [`Scheduler`] dispatch orchestration over the [`Clock`] + [`JobStore`] +
//! [`Committer`] seams, the injection-safe [`bind_last_run`] (`LAST_RUN()`), and the deterministic
//! [`run_id_for`] — has ZERO tokio / std-thread / global state and compiles to
//! `wasm32-unknown-unknown` with `--no-default-features`. The native [`daemon`] (tokio interval
//! loop + jitter + per-job timeout) is gated behind the default-on `native` feature; the
//! load-bearing wasm fence is the ABSENCE of `native` (tokio is `optional`), not a marker (the
//! t25/slack lesson).
//!
//! ## Time + run-id (no uncached heavy deps)
//! Instants are the project's standard `i64` epoch seconds (no `chrono`). The run-id is the
//! DETERMINISTIC `hash(job, scheduled_for)` over a vendored, wasm-clean SHA-256 (no `uuid`, no
//! randomness) — a retried fire for the same `scheduled_for` yields the same run-id, so the ledger
//! dedups it (better for idempotency than a random UUID).
//!
//! ## Confinement (boundary)
//! `qfs-cron` consumes `qfs-server` (the `Binding`/registry seam) + `qfs-exec` (the evaluator) +
//! `qfs-core`. It is a LEAF: only the terminal `qfs` binary depends on it (the serve composition
//! root, the cron sibling of `qfs-http`), so its feature-gated tokio dead-ends in the binary. It
//! does NOT depend on `qfs-runtime` — the real applier is the injected [`Committer`].

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(feature = "native")]
pub mod binding;
pub mod clock;
pub mod commit;
#[cfg(feature = "native")]
pub mod daemon;
pub mod lastrun;
pub mod policy;
pub mod schedule;
pub mod scheduler;
pub mod store;

#[cfg(feature = "native")]
pub use binding::{build_cron_binding, CronBinding, JobSetHandle, PolicyTableHandle};
// t35: re-export the qfs-server audit sink so the composition root wires one fired-plan ledger
// without naming `qfs-server` directly (keeping the binary's dep-allowlist unchanged — qfs-cron
// is already the legitimate qfs-server consumer for the JOB cause).
#[cfg(feature = "native")]
pub use clock::SystemClock;
pub use clock::{Clock, MockClock};
pub use commit::{CommitError, CommitOutcome, Committer};
#[cfg(feature = "native")]
pub use qfs_server::{AuditSink, PolicyDef, PolicyTable};
// Re-exported so the composition root's `Committer` impl can name the DO body's statement type
// without depending on `qfs-parser` directly.
#[cfg(feature = "native")]
pub use commit::RecordingCommitter;
#[cfg(feature = "native")]
pub use daemon::{run_daemon, scheduled_tick, DaemonConfig};
pub use lastrun::{bind_last_run, references_last_run};
pub use policy::MissedPolicy;
pub use qfs_parser::Statement;
pub use schedule::{CronExpr, CronField, Instant, Schedule, ScheduleError, Seconds};
pub use scheduler::{run_id_for, Dispatched, Scheduler};
pub use store::{
    JobBinding, JobStore, Lease, MemJobStore, PolicyRef, RunRecord, RunState, RunStatus, StoreError,
};

#[cfg(test)]
mod tests;
