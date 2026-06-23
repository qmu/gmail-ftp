//! The **sync→async execution bridge** (RFD-0001 §5/§9): adapt a driver's synchronous
//! [`cfs_plan::PlanApplier`] (the t09 effect seam every introspective `cfs_driver::Driver`
//! hands back via `applier()`) to the runtime-side async [`ApplyDriver`] the interpreter
//! dispatches through.
//!
//! ## Why a bridge
//! The introspective driver contract (t13) is **I/O-free and synchronous** so `cfs-plan`
//! and `cfs-driver` stay off `tokio` (the purity invariant, RFD §3). The lone impure seam
//! is `Driver::applier() -> &dyn PlanApplier`, a *synchronous* `apply(&mut self, node)`.
//! The interpreter, however, runs effects through the **async** [`ApplyDriver`] so it can
//! batch + parallelise. [`PlanApplierBridge`] is the thin adapter that lets a real driver's
//! synchronous apply leg run end-to-end under the async interpreter: it reconstructs an
//! owned [`EffectNode`] from each [`EffectInput`] and runs the blocking apply on a
//! `tokio` blocking thread (so file/socket I/O never stalls a runtime worker).
//!
//! ## Statelessness contract
//! The bridge wraps an **`Arc<A> where A: PlanApplier + Send + Sync`** and calls it through
//! a shared reference. `PlanApplier::apply` takes `&mut self`, but a real effect applier is
//! **stateless** (each call performs fresh World I/O and owns no mutable accumulator) — so
//! the bridge requires the applier to also be usable through `&self` by implementing the
//! marker [`SharedApplier`]. The in-house local-filesystem driver (t16) satisfies this: its
//! apply leg is pure I/O with no in-process mutable state. A *stateful* applier (a test
//! `RecordingApplier` that pushes to a `Vec`) is intentionally **not** bridgeable this way —
//! its state belongs behind its own synchronisation, not the apply hot path.
//!
//! No vendor SDK type crosses this boundary; the bridge trades only in owned
//! `cfs-plan`/`cfs-types` DTOs (RFD §9).

use std::sync::Arc;

use cfs_plan::EffectNode;

use crate::driver::{ApplyCx, ApplyDriver, EffectInput};
use crate::error::EffectError;
use crate::outcome::EffectOutput;

/// A synchronous effect applier that can be invoked through a **shared** reference — the
/// statelessness contract the [`PlanApplierBridge`] requires (see the module docs).
///
/// A real driver's apply leg performs fresh World I/O on every call and keeps no
/// in-process mutable accumulator, so `&self` apply is the honest shape; this trait is the
/// explicit opt-in that an applier is safe to drive concurrently from the async runtime.
pub trait SharedApplier: Send + Sync {
    /// Apply one effect node against the World through a shared reference.
    ///
    /// # Errors
    /// Returns a structured [`EffectError`] if the effect could not be applied. The
    /// error class (`retryable`/`terminal`/…) drives the interpreter's retry decision.
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError>;
}

/// Adapts a synchronous [`SharedApplier`] to the async [`ApplyDriver`] the interpreter
/// dispatches through (RFD §5/§9). The interpreter resolves a [`cfs_types::DriverId`] to
/// this bridge in its `DriverRegistry`; each grouped effect frontier is reconstructed into
/// owned [`EffectNode`]s and applied on a blocking thread.
///
/// Cloneable (holds an `Arc`) so the registry and scheduler can share it cheaply.
#[derive(Clone)]
pub struct PlanApplierBridge<A: SharedApplier> {
    inner: Arc<A>,
}

impl<A: SharedApplier> PlanApplierBridge<A> {
    /// Wrap a shared synchronous applier as an async [`ApplyDriver`].
    #[must_use]
    pub fn new(inner: Arc<A>) -> Self {
        Self { inner }
    }

    /// Borrow the wrapped applier (e.g. to reach its non-apply, introspective surface).
    #[must_use]
    pub fn inner(&self) -> &Arc<A> {
        &self.inner
    }
}

/// Reconstruct the owned [`EffectNode`] a [`SharedApplier`] expects from the runtime's
/// [`EffectInput`] projection. The runtime flattened the node into an input for batching;
/// the bridge rebuilds the node so the synchronous apply leg sees the same shape the
/// pure-side planner produced — including the planner's **`est_affected`**, carried through
/// [`EffectInput::est_affected`] so the reconstruction is faithful (a future driver that
/// pre-sizes a batch buffer or surfaces a progress estimate inside `apply_one` sees the
/// planner's honest estimate, not a degraded one). The applier still reports the *true*
/// affected count back on completion.
fn node_from_input(input: &EffectInput) -> EffectNode {
    let node = EffectNode::new(input.id, input.kind.clone(), input.target.clone())
        .irreversible(input.irreversible)
        // Carry the planner's estimate first; `with_args` only refines `Unknown`, so an
        // explicit estimate set here is preserved while a literal row batch still derives an
        // exact count when the planner left it `Unknown`.
        .with_affected(input.est_affected);
    if input.args.rows.is_empty() {
        node
    } else {
        node.with_args(input.args.clone())
    }
}

#[async_trait::async_trait]
impl<A: SharedApplier + 'static> ApplyDriver for PlanApplierBridge<A> {
    async fn apply_one(
        &self,
        effect: &EffectInput,
        _cx: &ApplyCx,
    ) -> Result<EffectOutput, EffectError> {
        let node = node_from_input(effect);
        let inner = Arc::clone(&self.inner);
        // The synchronous apply leg performs blocking World I/O (file reads/writes for the
        // local FS driver). Run it on a blocking thread so it never stalls a runtime worker;
        // a join failure (the blocking pool was shut down mid-commit) is a terminal,
        // non-retryable runtime fault for this leg — never a panic.
        match tokio::task::spawn_blocking(move || inner.apply_shared(&node)).await {
            Ok(result) => result,
            Err(join_err) => Err(EffectError::terminal(format!(
                "apply task failed to complete: {join_err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfs_plan::{DriverId, EffectKind, NodeId, Target, VfsPath};

    /// A stateless, no-I/O shared applier: echoes a fixed affected count. Proves the bridge
    /// drives a `&self` apply leg end-to-end without any live credentials or real I/O.
    struct EchoApplier {
        affected: u64,
    }

    impl SharedApplier for EchoApplier {
        fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
            Ok(EffectOutput::new(node.id, self.affected))
        }
    }

    fn input(id: u32, kind: EffectKind) -> EffectInput {
        let target = Target::new(DriverId::new("local"), VfsPath::new("/local/x"));
        EffectInput::from_node(&EffectNode::new(NodeId(id), kind, target))
    }

    #[tokio::test]
    async fn bridge_dispatches_shared_applier() {
        let bridge = PlanApplierBridge::new(Arc::new(EchoApplier { affected: 7 }));
        let out = bridge
            .apply_one(&input(1, EffectKind::Upsert), &ApplyCx::default())
            .await
            .unwrap();
        assert_eq!(out.id, NodeId(1));
        assert_eq!(out.affected, 7);
    }

    #[tokio::test]
    async fn bridge_batch_default_maps_over_singletons() {
        let bridge = PlanApplierBridge::new(Arc::new(EchoApplier { affected: 1 }));
        let effects = [input(1, EffectKind::Remove), input(2, EffectKind::Remove)];
        let results = bridge
            .apply_batch(EffectKind::Remove, &effects, &ApplyCx::default())
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().id, NodeId(1));
        assert_eq!(results[1].as_ref().unwrap().id, NodeId(2));
    }

    /// The bridge reconstructs the node so the synchronous leg sees the original kind,
    /// target, and irreversible flag the planner produced.
    #[tokio::test]
    async fn node_reconstruction_preserves_irreversible() {
        struct Inspect;
        impl SharedApplier for Inspect {
            fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
                assert!(node.irreversible, "REMOVE is inherently irreversible");
                assert_eq!(node.kind, EffectKind::Remove);
                Ok(EffectOutput::new(node.id, 0))
            }
        }
        let bridge = PlanApplierBridge::new(Arc::new(Inspect));
        bridge
            .apply_one(&input(3, EffectKind::Remove), &ApplyCx::default())
            .await
            .unwrap();
    }

    /// The planner's `est_affected` is carried through `EffectInput` and faithfully
    /// reconstructed onto the node the synchronous leg sees — not degraded to `Unknown`.
    #[tokio::test]
    async fn node_reconstruction_preserves_est_affected() {
        use cfs_plan::Affected;
        struct Inspect;
        impl SharedApplier for Inspect {
            fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
                assert_eq!(
                    node.est_affected,
                    Affected::AtMost(42),
                    "the planner's honest estimate must survive the bridge round-trip"
                );
                Ok(EffectOutput::new(node.id, 0))
            }
        }
        // Build an input from a node carrying an explicit AtMost estimate (a filter-driven
        // REMOVE whose true count is unknown until apply, but the planner bounds it).
        let target = Target::new(DriverId::new("local"), VfsPath::new("/local/x"));
        let node = EffectNode::new(NodeId(9), EffectKind::Remove, target)
            .with_affected(Affected::AtMost(42));
        let einput = EffectInput::from_node(&node);
        let bridge = PlanApplierBridge::new(Arc::new(Inspect));
        bridge
            .apply_one(&einput, &ApplyCx::default())
            .await
            .unwrap();
    }
}
