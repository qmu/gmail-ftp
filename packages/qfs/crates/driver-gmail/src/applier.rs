//! [`GmailApplier`] — the Gmail driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::GmailDriver`] hands back via `applier()`, and the
//! [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`.
//!
//! Stateless across the call: it holds the [`GmailClient`] behind an `Arc` and performs fresh
//! Gmail API I/O on every call — so it implements `SharedApplier` (`&self` apply), the
//! statelessness contract the bridge requires. Each effect is decoded to a [`GmailEffect`] and
//! dispatched to the client; the token is wholly behind the client (t19), never here.
//!
//! ## Idempotency / recovery (RFD §6)
//! `mail.send` is not naturally idempotent. The de-dupe key is a draft id: when `mail.send`
//! carries a draft id we send *that* draft (a retry re-sends the same draft id, not a fresh
//! message); when it carries draft content we **create a draft first**, then send it by id, so a
//! mid-send crash leaves a recoverable draft rather than a possible duplicate send. `UPSERT`
//! drafts are retry-safe (PUT by id). Trash + send are flagged irreversible upstream, so the
//! runtime never auto-retries them.

use std::sync::Arc;

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use crate::client::GmailClient;
use crate::effect::{draft_raw, GmailEffect};
use crate::error::GmailError;

/// The synchronous Gmail apply leg. Holds the [`GmailClient`] (the real auth-bearing client in
/// production, an in-memory mock in tests) behind an `Arc` so the leg is cheap to clone for the
/// runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct GmailApplier {
    client: Arc<dyn GmailClient>,
}

impl GmailApplier {
    /// Build an applier over `client`.
    #[must_use]
    pub fn new(client: Arc<dyn GmailClient>) -> Self {
        Self { client }
    }

    /// Apply one decoded [`GmailEffect`], returning the affected count. The single place Gmail
    /// API I/O happens.
    fn apply_effect(&self, effect: &GmailEffect) -> Result<u64, GmailError> {
        match effect {
            GmailEffect::CreateDraft { draft } => {
                let raw = draft_raw(draft)?;
                self.client.create_draft(&raw)?;
                Ok(1)
            }
            GmailEffect::CreateLabel { name } => {
                self.client.create_label(name)?;
                Ok(1)
            }
            GmailEffect::UpsertDraft { id, draft } => {
                let raw = draft_raw(draft)?;
                self.client.upsert_draft(id, &raw)?;
                Ok(1)
            }
            GmailEffect::ModifyLabels {
                message,
                add,
                remove,
            } => {
                self.client.modify_labels(message, add, remove)?;
                Ok(1)
            }
            GmailEffect::TrashMessage { id } => {
                self.client.trash_message(id)?;
                Ok(1)
            }
            GmailEffect::TrashThread { id } => {
                self.client.trash_thread(id)?;
                Ok(1)
            }
            GmailEffect::Send { draft_id, draft } => {
                // Resolve a draft id to send: an existing id (retry-safe), or create one first.
                let id = match draft_id {
                    Some(id) => id.clone(),
                    None => {
                        let draft = draft.as_ref().ok_or(GmailError::Mime {
                            reason: "mail.send carried neither a draft id nor draft content",
                        })?;
                        let raw = draft_raw(draft)?;
                        self.client.create_draft(&raw)?
                    }
                };
                self.client.send_draft(&id)?;
                Ok(1)
            }
        }
    }
}

impl SharedApplier for GmailApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect = GmailEffect::from_node(node)?;
        let affected = self.apply_effect(&effect)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for GmailApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The Gmail applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`GmailError`] is reduced to the plan
    /// crate's owned `(id, reason)` shape — secret-free by construction — so no driver type
    /// leaks into `qfs-plan`.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect =
            GmailEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        let affected = self
            .apply_effect(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
