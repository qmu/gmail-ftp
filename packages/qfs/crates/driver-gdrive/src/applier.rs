//! [`DriveApplier`] — the Drive driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::GDriveDriver`] hands back via `applier()`, and the
//! [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`.
//!
//! Stateless across the call: it holds the [`GDriveClient`] behind an `Arc` and performs fresh
//! Drive API I/O on every call — so it implements `SharedApplier` (`&self` apply), the
//! statelessness contract the bridge requires. Each effect is decoded to a [`DriveEffect`] and
//! dispatched to the client; the token is wholly behind the client (t19), never here.
//!
//! ## Idempotency / recovery (RFD §6)
//! `UPSERT` is the retry-safe write: a content replace by id (PATCH-by-media) is idempotent, and
//! a resumable create resumes on the same session URI rather than duplicating a file. `REMOVE`
//! defaults to **trash** (recoverable) — a permanent `Delete` requires an explicit `hard_delete`
//! flag and is irreversible, so the runtime never auto-retries it. `mv` is the planner's
//! copy→verify→delete DAG; this leg applies a single metadata move (`Move`) or the server-side
//! `Copy`, each an irreducible step the ledger records.

use std::sync::Arc;

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use crate::client::GDriveClient;
use crate::effect::DriveEffect;
use crate::error::DriveError;

/// The synchronous Drive apply leg. Holds the [`GDriveClient`] (the real auth-bearing client in
/// production, an in-memory mock in tests) behind an `Arc` so the leg is cheap to clone for the
/// runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct DriveApplier {
    client: Arc<dyn GDriveClient>,
}

impl DriveApplier {
    /// Build an applier over `client`.
    #[must_use]
    pub fn new(client: Arc<dyn GDriveClient>) -> Self {
        Self { client }
    }

    /// Apply one decoded [`DriveEffect`], returning the affected count. The single place Drive
    /// API write I/O happens.
    fn apply_effect(&self, effect: &DriveEffect) -> Result<u64, DriveError> {
        match effect {
            DriveEffect::Upload {
                parent,
                name,
                mime,
                bytes,
            } => {
                self.client.upload(parent, name, mime, bytes)?;
                Ok(1)
            }
            DriveEffect::Update { id, mime, bytes } => {
                self.client.update_content(id, mime, bytes)?;
                Ok(1)
            }
            DriveEffect::Move {
                id,
                new_name,
                add_parents,
                remove_parents,
            } => {
                self.client
                    .modify_file(id, new_name.as_deref(), add_parents, remove_parents)?;
                Ok(1)
            }
            DriveEffect::Copy { id, parent, name } => {
                self.client.copy_file(id, parent, name)?;
                Ok(1)
            }
            DriveEffect::Trash { id } => {
                self.client.trash(id)?;
                Ok(1)
            }
            DriveEffect::Delete { id } => {
                self.client.delete(id)?;
                Ok(1)
            }
        }
    }
}

impl SharedApplier for DriveApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect = DriveEffect::from_node(node)?;
        let affected = self.apply_effect(&effect)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for DriveApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The Drive applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`DriveError`] is reduced to the plan
    /// crate's owned `(id, reason)` shape — secret-free by construction — so no driver type leaks
    /// into `qfs-plan`.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect =
            DriveEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        let affected = self
            .apply_effect(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
