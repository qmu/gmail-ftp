//! [`GitHubApplier`] — the GitHub driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::GitHubDriver`] hands back via `applier()`, and the
//! [`cfs_runtime::SharedApplier`] the runtime's [`cfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`.
//!
//! Stateless across the call: it holds the [`GitHubClient`] behind an `Arc` and performs fresh
//! GitHub API I/O on every call — so it implements `SharedApplier` (`&self` apply), the
//! statelessness contract the bridge requires. Each effect is decoded to a [`GitHubEffect`] and
//! dispatched to the client; the PAT is wholly behind the client, never here.
//!
//! ## Idempotency / recovery (RFD §6)
//! A non-idempotent POST (comment/open/create/review/dispatch) and the irreversible `merge` are
//! reported **terminal** on a transient failure so the interpreter never re-sends them
//! (at-least-once); a PATCH is likewise not retried (not guaranteed idempotent). A `DELETE` is
//! idempotent on the wire, so a transient failure on it is retryable. The retry-class decision is
//! derived from the effect's HTTP method `is_retry_safe` plus the explicit POST guard.

use std::sync::Arc;

use cfs_http_core::HttpMethod;
use cfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use cfs_runtime::{EffectError, EffectOutput, SharedApplier};

use crate::client::GitHubClient;
use crate::effect::GitHubEffect;

/// The synchronous GitHub apply leg. Holds the [`GitHubClient`] (the real auth-bearing client in
/// production, an in-memory mock in tests) behind an `Arc` so the leg is cheap to clone for the
/// runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct GitHubApplier {
    client: Arc<dyn GitHubClient>,
}

impl GitHubApplier {
    /// Build an applier over `client`.
    #[must_use]
    pub fn new(client: Arc<dyn GitHubClient>) -> Self {
        Self { client }
    }

    /// Borrow the client (e.g. for the read leg).
    #[must_use]
    pub fn client(&self) -> &Arc<dyn GitHubClient> {
        &self.client
    }

    /// The HTTP method an effect issues on the wire — the retry-class input. POST/PATCH are not
    /// retry-safe; DELETE/PUT are. Mirrors [`crate::client`]'s actual method choice.
    fn method_of(effect: &GitHubEffect) -> HttpMethod {
        match effect {
            GitHubEffect::OpenIssue { .. }
            | GitHubEffect::OpenPull { .. }
            | GitHubEffect::PostComment { .. }
            | GitHubEffect::CreateRelease { .. }
            | GitHubEffect::CreateBranch { .. }
            | GitHubEffect::Dispatch { .. }
            | GitHubEffect::Review { .. } => HttpMethod::Post,
            GitHubEffect::PatchIssue { .. } | GitHubEffect::PatchPull { .. } => HttpMethod::Patch,
            GitHubEffect::DeleteComment { .. }
            | GitHubEffect::DeleteRelease { .. }
            | GitHubEffect::DeleteBranch { .. } => HttpMethod::Delete,
            GitHubEffect::Merge { .. } => HttpMethod::Put,
        }
    }
}

impl SharedApplier for GitHubApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect =
            GitHubEffect::from_node(node).map_err(|e| EffectError::terminal(e.to_string()))?;
        // `merge` is a PUT on the wire (idempotent at the API), but it is irreversible and uses
        // optimistic concurrency on the head SHA — so a transient failure is reported terminal,
        // never auto-retried (RFD §6 — do not re-attempt an irreversible merge).
        let retry_safe = Self::method_of(&effect).is_retry_safe()
            && !effect.is_at_least_once_post()
            && !matches!(effect, GitHubEffect::Merge { .. });
        let affected = self
            .client
            .apply(&effect)
            .map_err(|e| e.into_effect_error(retry_safe))?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for GitHubApplier {
    /// The introspective `cfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The GitHub applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`crate::error::GitHubError`] is reduced to
    /// the plan crate's owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect =
            GitHubEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        let affected = self
            .client
            .apply(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
