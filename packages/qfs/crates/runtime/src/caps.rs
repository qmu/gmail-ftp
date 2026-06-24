//! Apply-time capability gating (RFD-0001 §5/§10) and concurrency limits.
//!
//! The parse-time capability gate is t13 (`qfs_driver::check_capability`); this is the
//! **defense-in-depth re-check** the interpreter performs immediately before dispatching an
//! effect, so a plan that slipped past parsing (or was constructed programmatically) still
//! cannot reach the World with an ungranted `(driver, verb)`. The check keys on owned
//! identity only — a [`DriverId`] plus the effect's verb label — never a credential.

use std::collections::HashSet;

use qfs_plan::{EffectKind, Target};
use qfs_types::DriverId;

/// The set of `(driver, verb)` grants in force for a commit — the least-privilege envelope
/// the interpreter enforces at apply time. An effect whose `(driver, verb)` is absent is
/// rejected with a structured `capability-denied` error **before** the driver is called.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    grants: HashSet<(DriverId, String)>,
    allow_all: bool,
}

impl CapabilitySet {
    /// An empty set — every effect is denied. The safe default for unattended runs.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A set that grants everything — used by trusted callers and most tests that are not
    /// exercising the gate itself. Explicit, never the default.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            grants: HashSet::new(),
            allow_all: true,
        }
    }

    /// Grant `verb` on `driver` (builder form). The verb is the stable [`EffectKind`] label
    /// (`READ`/`INSERT`/`CALL`/…) so the grant set is owned, vendor-free data.
    #[must_use]
    pub fn grant(mut self, driver: DriverId, kind: &EffectKind) -> Self {
        self.grants.insert((driver, kind.label().to_string()));
        self
    }

    /// Whether this effect (its target driver + kind) is permitted.
    #[must_use]
    pub fn allows(&self, target: &Target, kind: &EffectKind) -> bool {
        self.allow_all
            || self
                .grants
                .contains(&(target.driver.clone(), kind.label().to_string()))
    }
}

/// Two-level concurrency caps (RFD §6 backpressure): a `global` ceiling on driver groups in
/// flight across the whole commit, and a `per_driver` ceiling so one driver cannot consume
/// the whole budget (respecting upstream rate limits). Config-driven so a wide DAG frontier
/// never spawns unbounded tasks or exhausts file descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyLimits {
    /// Max driver-groups dispatched concurrently across all drivers.
    pub global: usize,
    /// Max driver-groups dispatched concurrently *per driver id*.
    pub per_driver: usize,
}

impl ConcurrencyLimits {
    /// Construct limits, clamping each to at least 1 (a zero limit would deadlock the
    /// scheduler — a semaphore with no permits never admits a group).
    #[must_use]
    pub fn new(global: usize, per_driver: usize) -> Self {
        Self {
            global: global.max(1),
            per_driver: per_driver.max(1),
        }
    }
}

impl Default for ConcurrencyLimits {
    /// A conservative default: a modest global fan-out, modest per-driver fan-out.
    fn default() -> Self {
        Self::new(8, 4)
    }
}

/// Per-leg timeout + retry policy (RFD §6 idempotency/observability). Retries apply **only**
/// to retryable, non-`irreversible` legs — the runtime never auto-retries an irreversible
/// effect (`REMOVE`, `CALL mail.send`) even on a transient error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Max attempts for a retryable, non-irreversible leg (1 = no retry).
    pub max_attempts: u32,
    /// Per-leg timeout in milliseconds (`None` = no timeout). A leg that exceeds it fails
    /// with [`EffectError::TimedOut`](crate::EffectError::TimedOut).
    pub timeout_millis: Option<u64>,
}

impl RetryPolicy {
    /// Construct a policy, clamping `max_attempts` to at least 1.
    #[must_use]
    pub fn new(max_attempts: u32, timeout_millis: Option<u64>) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            timeout_millis,
        }
    }
}

impl Default for RetryPolicy {
    /// A conservative default: up to 3 attempts on retryable legs, no wall-clock timeout
    /// (tests opt into a timeout explicitly so they stay deterministic).
    fn default() -> Self {
        Self::new(3, None)
    }
}
