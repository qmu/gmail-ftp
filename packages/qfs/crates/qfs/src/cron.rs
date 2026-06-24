//! The `qfs serve` **cron composition** (t33): the binary wires the JOB scheduler binding.
//!
//! Like the HTTP binding, `qfs-cron` is a LEAF that consumes `qfs-server` (the registry +
//! reconcile seam) AND `qfs-exec` (the evaluator) — composing it HERE (the terminal binary) keeps
//! `qfs-cmd` off it and lets `qfs-cron`'s feature-gated tokio daemon dead-end in the sink. The
//! binary builds the `CronBinding` (reconciled by the runtime from `/server/jobs`), a binary-local
//! [`LedgerJobStore`] that reads the binding's live JOB set, a [`PreviewCommitter`] that builds the
//! DO plan via `qfs_exec::build_plan`, and spawns the native daemon loop.
//!
//! ## Parked wiring (carry-over, recorded honestly)
//!   * **The live-driver applier**: the committer here builds the DO plan and applies it through a
//!     `RecordingApplier` (the PREVIEW path) — the SAME state the HTTP read drivers are in at this
//!     stage (the deployment registers real drivers; an unregistered source is a structured error,
//!     never a panic). Routing a JOB's real-effect COMMIT through the runtime `Interpreter` with
//!     live drivers is the deeper wiring deferred to the E2/E4 runtime integration (t34/t35/t38).
//!   * **Durable run state**: the run state / lease / ledger live in this binary-local store
//!     in-memory; persisting them through the `/server` store (EC2) or a Durable Object (CF) is
//!     the parked deployment detail. The scheduler's contract is unchanged — it only sees the
//!     `JobStore` trait.

use std::collections::BTreeMap;
use std::sync::Mutex;

use qfs_core::Engine;
use qfs_cron::{
    CommitError, CommitOutcome, Committer, JobBinding, JobSetHandle, JobStore, Lease, PolicyRef,
    RunRecord, RunState, RunStatus, StoreError,
};

/// A binary-local [`JobStore`]: `load_enabled` reads the [`CronBinding`]'s live JOB set (the
/// registry projection the runtime reconciles); run state / lease / ledger are held in-memory
/// (the durable `/server`/DO persistence is the parked deployment detail).
pub struct LedgerJobStore {
    jobs: JobSetHandle,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    state: BTreeMap<String, RunState>,
    committed: BTreeMap<String, ()>,
    leases: BTreeMap<String, ()>,
    ledger: Vec<RunRecord>,
}

fn key(job: &str, run_id: &str) -> String {
    format!("{job}\u{0}{run_id}")
}

impl LedgerJobStore {
    /// Build a store over the cron binding's shared JOB-set handle.
    #[must_use]
    pub fn new(jobs: JobSetHandle) -> Self {
        Self {
            jobs,
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl JobStore for LedgerJobStore {
    fn load_enabled(&self) -> Result<Vec<JobBinding>, StoreError> {
        let snapshot = self
            .jobs
            .read()
            .map_err(|_| StoreError::Unavailable("cron job set lock poisoned".to_string()))?;
        Ok(snapshot.iter().filter(|j| j.enabled).cloned().collect())
    }

    fn run_state(&self, job: &str) -> Result<RunState, StoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("ledger store poisoned".to_string()))?;
        Ok(g.state.get(job).cloned().unwrap_or_default())
    }

    fn is_committed(&self, job: &str, run_id: &str) -> Result<bool, StoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("ledger store poisoned".to_string()))?;
        Ok(g.committed.contains_key(&key(job, run_id)))
    }

    fn acquire_lease(&self, job: &str, run_id: &str, _ttl: i64) -> Result<Lease, StoreError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("ledger store poisoned".to_string()))?;
        let k = key(job, run_id);
        if g.leases.contains_key(&k) {
            return Ok(Lease {
                acquired: false,
                job: job.to_string(),
                run_id: run_id.to_string(),
            });
        }
        g.leases.insert(k, ());
        Ok(Lease {
            acquired: true,
            job: job.to_string(),
            run_id: run_id.to_string(),
        })
    }

    fn release_lease(&self, job: &str, run_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.leases.remove(&key(job, run_id));
        }
    }

    fn record_run(&self, record: RunRecord) -> Result<(), StoreError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("ledger store poisoned".to_string()))?;
        let entry = g.state.entry(record.job.clone()).or_default();
        match record.status {
            RunStatus::Success => {
                entry.last_run_at = Some(record.scheduled_for);
                entry.last_status = RunStatus::Success;
                entry.last_plan_hash = record.plan_hash.clone();
                entry.consecutive_failures = 0;
                g.committed.insert(key(&record.job, &record.run_id), ());
            }
            RunStatus::Failed => {
                entry.last_status = RunStatus::Failed;
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            }
            RunStatus::Disabled => entry.last_status = RunStatus::Disabled,
            // `Never` and any future variant: no state change. NOTE: the wildcard is MANDATORY
            // here — RunStatus is `#[non_exhaustive]` and this is an out-of-crate match, so a
            // no-catch-all shape (the safer one that surfaces a new variant at compile time) is
            // not expressible for LedgerJobStore, only for the in-crate MemJobStore.
            _ => {}
        }
        g.ledger.push(record);
        Ok(())
    }
}

/// The committer the binary injects: builds the DO plan via `qfs_exec::build_plan` over the serve
/// engine and applies it. At t33 this is the PREVIEW path (a `RecordingApplier`); the live-driver
/// runtime-interpreter applier is the parked carry-over (see the module doc). Reuses `qfs-cron`'s
/// own `RecordingCommitter` shape — the binary just supplies the serve engine.
pub struct PreviewCommitter {
    inner: qfs_cron::RecordingCommitter,
}

impl PreviewCommitter {
    /// Build a committer wired with the t35 policy gate: the live `/server/policies` table the
    /// JOB's bound policy ref resolves against, and the fired-plan audit sink (one
    /// `FiredPlanRecord` per fire). A denied JOB plan aborts atomically (zero effects).
    #[must_use]
    pub fn with_policy(
        engine: Engine,
        policies: qfs_cron::PolicyTableHandle,
        audit: std::sync::Arc<qfs_cron::AuditSink>,
    ) -> Self {
        Self {
            inner: qfs_cron::RecordingCommitter::with_engine(engine)
                .with_policies(policies)
                .with_audit(audit),
        }
    }
}

impl Committer for PreviewCommitter {
    fn commit(
        &self,
        stmt: &qfs_cron::Statement,
        policy: &PolicyRef,
    ) -> Result<CommitOutcome, CommitError> {
        self.inner.commit(stmt, policy)
    }
}
