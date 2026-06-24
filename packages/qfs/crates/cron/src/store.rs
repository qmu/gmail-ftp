//! [`JobStore`] — the durable persistence seam (RFD §8): enabled JOB rows, per-JOB run state, the
//! single-flight lease, and the run-id-keyed audit ledger. Backed by the `/server` store on EC2
//! (the binary's `LedgerJobStore`) and by a Durable Object on Cloudflare; tests use [`MemJobStore`].
//!
//! The pure scheduler core knows only the trait — it never reaches the `/server` runtime, so this
//! crate stays off `qfs-runtime` and off any wall-clock/global state on the wasm path.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use qfs_core::PlanSpec;

use crate::policy::MissedPolicy;
use crate::schedule::{Instant, Schedule};

/// A reference to the handler `POLICY` / capability set a JOB commits under (RFD §10). The
/// scheduler **threads** this into the commit call (least privilege) and never widens it. The
/// full POLICY engine is a sibling ticket; here it is the handle name only (never a credential).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRef {
    /// The policy handle name (a `/server/policies` row), or empty for the default (no widening).
    pub policy: String,
}

impl PolicyRef {
    /// A policy reference to a named handle.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            policy: name.into(),
        }
    }
}

/// An owned, vendor-free JOB binding read from `/server/jobs` (RFD §8). `plan` is the t31
/// canonical [`PlanSpec`] body (the parsed-but-unevaluated `DO <plan>`, a `-> Plan` thunk honoring
/// the purity invariant), rehydrated via `from_canonical` — never a live `Plan`.
#[derive(Debug, Clone)]
pub struct JobBinding {
    /// The JOB name (the `/server/jobs` row key).
    pub name: String,
    /// The firing cadence.
    pub schedule: Schedule,
    /// The `DO <plan>` body, the canonical t31 spec (rehydrated, no re-parse).
    pub plan: PlanSpec,
    /// The handler policy / capability set the commit runs under (never widened).
    pub policy: PolicyRef,
    /// What to do across missed due times.
    pub missed: MissedPolicy,
    /// Whether the JOB is enabled (a circuit-breaker auto-disable clears this).
    pub enabled: bool,
}

/// The outcome of the last fire of a JOB (RFD §6 audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RunStatus {
    /// Never fired.
    #[default]
    Never,
    /// The last fire committed successfully.
    Success,
    /// The last fire failed to commit (leaves `last_run_at` unmoved — the window re-covers).
    Failed,
    /// The JOB was auto-disabled by the circuit-breaker after repeated failures.
    Disabled,
}

/// The durable per-JOB run state backing `LAST_RUN()` (RFD §8). `last_run_at` is the high-water
/// mark — the `scheduled_for` boundary of the last **successful** commit (advanced only on
/// success). `consecutive_failures` drives the circuit-breaker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// The `scheduled_for` boundary of the last successful commit; `None` until the first.
    pub last_run_at: Option<Instant>,
    /// The status of the last fire.
    pub last_status: RunStatus,
    /// The plan fingerprint of the last successful commit (counts/hashes only — never payload).
    pub last_plan_hash: Option<String>,
    /// Consecutive failures since the last success (the circuit-breaker counter).
    pub consecutive_failures: u32,
}

/// One audit-ledger entry per fire (RFD §6/§10): counts and hashes only — **no secrets, no plan
/// payloads**. The log-scrub test asserts this projection carries no DO source / token text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The JOB name.
    pub job: String,
    /// The deterministic run-id = hash(job, scheduled_for).
    pub run_id: String,
    /// The boundary this fire was scheduled for (epoch seconds).
    pub scheduled_for: Instant,
    /// The status of this fire.
    pub status: RunStatus,
    /// How many effects the commit applied (the safe-to-log count).
    pub applied_count: u64,
    /// The committed plan fingerprint (hash only), if the commit succeeded.
    pub plan_hash: Option<String>,
    /// A short, secret-free failure note (the apply error reason, already secret-free), if failed.
    pub failure_note: Option<String>,
}

impl RunRecord {
    /// A one-line, secret-free audit projection (the structured-log form). Counts + status +
    /// run-id only — never the DO body, never a credential.
    #[must_use]
    pub fn log_line(&self) -> String {
        format!(
            "job={} run_id={} scheduled_for={} status={:?} applied={}",
            self.job, self.run_id, self.scheduled_for, self.status, self.applied_count
        )
    }
}

/// A single-flight lease over a JOB run (RFD §6 exactly-once-ish): holding the lease for a
/// `(job, run_id)` is what lets exactly one of two concurrent dispatches commit. `acquired`
/// false means another dispatch holds it — the caller no-ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Whether this caller acquired the lease (false → another holder, no-op).
    pub acquired: bool,
    /// The job the lease is over.
    pub job: String,
    /// The run-id the lease is over.
    pub run_id: String,
}

/// A structured, secret-free store error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A backing-store access failed (lock poisoned, snapshot unavailable, …).
    #[error("job store unavailable: {0}")]
    Unavailable(String),
    /// A stored JOB row could not be rehydrated into a binding (bad canonical body / schedule).
    #[error("job {job:?} could not be loaded: {reason}")]
    BadBinding {
        /// The offending JOB name.
        job: String,
        /// A secret-free reason.
        reason: String,
    },
}

/// The durable persistence seam. All methods are synchronous (the pure core drives them); a native
/// backing store performs its I/O behind this trait, never inside the scheduler.
pub trait JobStore {
    /// Load the enabled JOB bindings (disabled / circuit-broken jobs are excluded).
    ///
    /// # Errors
    /// [`StoreError`] if the store is unavailable or a row cannot be rehydrated.
    fn load_enabled(&self) -> Result<Vec<JobBinding>, StoreError>;

    /// The durable run state for `job` (backs `LAST_RUN()`).
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] if the store cannot be read.
    fn run_state(&self, job: &str) -> Result<RunState, StoreError>;

    /// Whether `(job, run_id)` already committed successfully (idempotency dedup): a retried
    /// dispatch with the same run-id after success must be a no-op.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] if the store cannot be read.
    fn is_committed(&self, job: &str, run_id: &str) -> Result<bool, StoreError>;

    /// Acquire the single-flight lease for `(job, run_id)` with time-to-live `ttl` seconds. If
    /// another holder has a live lease, returns a [`Lease`] with `acquired = false`.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] if the store cannot be written.
    fn acquire_lease(&self, job: &str, run_id: &str, ttl: Instant) -> Result<Lease, StoreError>;

    /// Release the lease for `(job, run_id)` (best-effort; the TTL also expires it).
    fn release_lease(&self, job: &str, run_id: &str);

    /// Record one fire's audit entry AND advance the durable run state per the record's status
    /// (success → advance `last_run_at` to `scheduled_for`, store `plan_hash`, reset the failure
    /// counter; failure → leave `last_run_at`, bump the counter).
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] if the store cannot be written.
    fn record_run(&self, record: RunRecord) -> Result<(), StoreError>;
}

/// An in-process [`JobStore`] for tests (no live creds, no `/server` runtime): a `Mutex`-guarded
/// map of bindings, run state, the committed-run-id set, and the held leases. Deterministic.
#[derive(Debug, Default)]
pub struct MemJobStore {
    inner: Mutex<MemInner>,
}

#[derive(Debug, Default)]
struct MemInner {
    bindings: Vec<JobBindingRow>,
    state: BTreeMap<String, RunState>,
    /// Committed `(job, run_id)` pairs (idempotency dedup).
    committed: BTreeMap<String, ()>,
    /// Held leases keyed by `(job, run_id)` (the test store ignores TTL expiry — a held lease is
    /// held until released, which is sufficient to model single-flight concurrency).
    leases: BTreeMap<String, ()>,
    /// The audit ledger (every recorded RunRecord, in order).
    pub ledger: Vec<RunRecord>,
}

/// A stored binding row (clone-on-load).
#[derive(Debug, Clone)]
struct JobBindingRow {
    binding: JobBinding,
}

fn key(job: &str, run_id: &str) -> String {
    format!("{job}\u{0}{run_id}")
}

impl MemJobStore {
    /// An empty test store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a JOB binding the store will load.
    pub fn put_binding(&self, binding: JobBinding) {
        if let Ok(mut g) = self.inner.lock() {
            g.bindings.retain(|r| r.binding.name != binding.name);
            g.bindings.push(JobBindingRow { binding });
        }
    }

    /// Seed the durable run state for a JOB (e.g. to test an existing `last_run_at`).
    pub fn put_state(&self, job: &str, state: RunState) {
        if let Ok(mut g) = self.inner.lock() {
            g.state.insert(job.to_string(), state);
        }
    }

    /// A snapshot of the audit ledger (the recorded RunRecords, in order).
    #[must_use]
    pub fn ledger(&self) -> Vec<RunRecord> {
        self.inner
            .lock()
            .map(|g| g.ledger.clone())
            .unwrap_or_default()
    }

    /// Whether a lease is currently held for `(job, run_id)` (test introspection).
    #[must_use]
    pub fn lease_held(&self, job: &str, run_id: &str) -> bool {
        self.inner
            .lock()
            .map(|g| g.leases.contains_key(&key(job, run_id)))
            .unwrap_or(false)
    }
}

impl JobStore for MemJobStore {
    fn load_enabled(&self) -> Result<Vec<JobBinding>, StoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("mem store poisoned".to_string()))?;
        Ok(g.bindings
            .iter()
            .filter(|r| r.binding.enabled)
            .map(|r| r.binding.clone())
            .collect())
    }

    fn run_state(&self, job: &str) -> Result<RunState, StoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("mem store poisoned".to_string()))?;
        Ok(g.state.get(job).cloned().unwrap_or_default())
    }

    fn is_committed(&self, job: &str, run_id: &str) -> Result<bool, StoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("mem store poisoned".to_string()))?;
        Ok(g.committed.contains_key(&key(job, run_id)))
    }

    fn acquire_lease(&self, job: &str, run_id: &str, _ttl: Instant) -> Result<Lease, StoreError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("mem store poisoned".to_string()))?;
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
            .map_err(|_| StoreError::Unavailable("mem store poisoned".to_string()))?;
        // Advance durable state per the record's status (the LAST_RUN advance ordering).
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
                // last_run_at unmoved (the window re-covers next tick).
                entry.last_status = RunStatus::Failed;
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            }
            RunStatus::Disabled => {
                entry.last_status = RunStatus::Disabled;
            }
            RunStatus::Never => {}
        }
        g.ledger.push(record);
        Ok(())
    }
}
