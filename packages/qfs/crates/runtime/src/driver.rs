//! The runtime-side **batch driver** seam (RFD-0001 §5/§9) and the [`DriverRegistry`]
//! the interpreter resolves effects through.
//!
//! This is the consumer-side narrow async trait the interpreter calls; it is the apply-time
//! counterpart of the introspective `qfs_driver::Driver` (t13). Keeping it here — rather
//! than in `qfs-driver` — is what lets the interpreter own async/tokio while `qfs-plan`
//! and `qfs-driver` stay I/O-free (the purity invariant, RFD §3). A real E4 driver bridges
//! its synchronous `qfs_plan::PlanApplier` to [`ApplyDriver`] with a thin adapter; tests
//! use an in-memory mock. **No vendor SDK type crosses this boundary** — the runtime keys
//! batching on the owned [`DriverId`] + `EffectKind`, never a driver-internal type (§9).

use std::collections::HashMap;
use std::sync::Arc;

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::DriverId;

use crate::error::EffectError;
use crate::outcome::EffectOutput;

/// The per-call context handed to a driver's batch entrypoint. Carries only
/// apply-coordination metadata — **never** credentials or tokens (those live behind the
/// driver's own construction, RFD §10). Reserved for E4 to thread a request id / deadline;
/// kept minimal at E0 so the seam is stable.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ApplyCx {
    /// Whether this is the final (post-retry-exhaustion) attempt — a hint a driver may use
    /// to widen its own error detail. Advisory only.
    pub last_attempt: bool,
}

/// One effect presented to a driver's batch entrypoint — an **owned** view of the plan
/// node (RFD §9: owned DTOs, no vendor types). The driver fans its results back aligned to
/// the input order, so identity is carried by position *and* by the explicit [`EffectInput::id`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EffectInput {
    /// The plan-local node identity (for result fan-out and the ledger).
    pub id: qfs_plan::NodeId,
    /// What the effect does (the batch is homogeneous in `kind`; provided for convenience).
    pub kind: EffectKind,
    /// Where the effect lands.
    pub target: qfs_plan::Target,
    /// The rows the effect writes (empty for reads/filter-removes).
    pub args: qfs_types::RowBatch,
    /// Whether this leg is irreversible (the runtime already vetoed retries; the driver
    /// may use it for its own safety checks).
    pub irreversible: bool,
    /// The planner's honest affected-row estimate (`Exact`/`AtMost`/`Unknown`). Carried
    /// through so the reconstructed node is faithful — a driver that pre-sizes a batch buffer
    /// or surfaces a progress estimate inside `apply_one` sees the planner's estimate, not a
    /// degraded one. The applier still reports the *true* affected count back on completion.
    pub est_affected: qfs_plan::Affected,
}

impl EffectInput {
    /// Build a batch input from a plan node (the runtime's owned projection of it).
    #[must_use]
    pub fn from_node(node: &EffectNode) -> Self {
        Self {
            id: node.id,
            kind: node.kind.clone(),
            target: node.target.clone(),
            args: node.args.clone(),
            irreversible: node.irreversible,
            est_affected: node.est_affected,
        }
    }
}

/// The runtime-side driver contract: apply a **homogeneous batch** of effects (all sharing
/// the same `(DriverId, EffectKind)` grouping key) in one call (RFD §6 auto-batching). The
/// interpreter coalesces a whole DAG frontier into these calls; a driver that has no native
/// batch endpoint maps over singletons via the provided default — **batching is an
/// interpreter contract, not a driver requirement**.
///
/// `Send + Sync` so the registry can hold `Arc<dyn ApplyDriver>` and the scheduler can
/// dispatch groups across tasks.
#[async_trait::async_trait]
pub trait ApplyDriver: Send + Sync {
    /// Apply a batch of same-`kind` effects, returning one result **per input, in input
    /// order** (the runtime fans these back to per-effect ledger entries by position).
    ///
    /// The default implementation maps over singletons via [`ApplyDriver::apply_one`], so a
    /// driver with no true batch endpoint only implements `apply_one`. A driver with a
    /// native batch endpoint (Gmail `messages.batchModify`, SQL multi-row `INSERT`)
    /// overrides this to collapse the N calls into one.
    async fn apply_batch(
        &self,
        kind: EffectKind,
        effects: &[EffectInput],
        cx: &ApplyCx,
    ) -> Vec<Result<EffectOutput, EffectError>> {
        let _ = kind;
        let mut out = Vec::with_capacity(effects.len());
        for e in effects {
            out.push(self.apply_one(e, cx).await);
        }
        out
    }

    /// Apply a single effect — the per-leg fallback the default [`ApplyDriver::apply_batch`]
    /// maps over. A driver that overrides `apply_batch` need not do anything useful here,
    /// but the default trait still requires it; most E4 drivers implement this and inherit
    /// batching for free.
    async fn apply_one(
        &self,
        effect: &EffectInput,
        cx: &ApplyCx,
    ) -> Result<EffectOutput, EffectError>;
}

/// The apply-time driver registry (G2): maps a [`DriverId`] to the [`ApplyDriver`] that
/// services its effects. The interpreter resolves every grouped frontier through this — it
/// never holds a concrete driver type, only `Arc<dyn ApplyDriver>` trait objects.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: HashMap<DriverId, Arc<dyn ApplyDriver>>,
}

impl DriverRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the driver for `id`. Builder-style for terse setup.
    #[must_use]
    pub fn with(mut self, id: DriverId, driver: Arc<dyn ApplyDriver>) -> Self {
        self.drivers.insert(id, driver);
        self
    }

    /// Register the driver for `id` (mutating form).
    pub fn register(&mut self, id: DriverId, driver: Arc<dyn ApplyDriver>) {
        self.drivers.insert(id, driver);
    }

    /// Resolve the driver for `id`, if registered.
    #[must_use]
    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn ApplyDriver>> {
        self.drivers.get(id).cloned()
    }
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("drivers", &self.drivers.keys().collect::<Vec<_>>())
            .finish()
    }
}
