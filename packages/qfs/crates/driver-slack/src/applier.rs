//! [`SlackApplier`] — the Slack driver's synchronous apply leg (RFD-0001 §6). It is the lone impure
//! seam the introspective [`crate::SlackDriver`] hands back via `applier()`, and (under the
//! `runtime` feature) the [`qfs_runtime::SharedApplier`] the runtime's
//! [`qfs_runtime::PlanApplierBridge`] drives under `COMMIT`.
//!
//! Stateless across the call: it holds the [`SlackClient`] behind an `Arc` and performs fresh Slack
//! Web-API I/O on every call. Each effect is decoded to a [`SlackEffect`] and dispatched to the
//! client; the bot token is wholly behind the client, never here.
//!
//! ## Idempotency / recovery (RFD §6)
//! `chat.postMessage` is non-idempotent: on a transient/ambiguous failure it is reported
//! **terminal** so the interpreter never re-sends it (at-least-once — do not auto-retry a post on
//! an ambiguous timeout). `reactions.add`/`pins.add` are naturally idempotent; their already-done
//! class is swallowed inside the client, so a genuine failure here is reported faithfully. The
//! [`crate::client::BodyErrorRule`] maps Slack's HTTP-200 `ok:false` to a terminal application
//! error inside the seam, so it never looks like a retryable transport hiccup.

use std::sync::Arc;

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};

use crate::client::SlackClient;
use crate::effect::SlackEffect;

/// The synchronous Slack apply leg. Holds the [`SlackClient`] (the real auth-bearing client in
/// production, an in-memory mock in tests) behind an `Arc` so the leg is cheap to clone for the
/// runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct SlackApplier {
    client: Arc<dyn SlackClient>,
}

impl SlackApplier {
    /// Build an applier over `client`.
    #[must_use]
    pub fn new(client: Arc<dyn SlackClient>) -> Self {
        Self { client }
    }

    /// Borrow the client (e.g. for the read leg).
    #[must_use]
    pub fn client(&self) -> &Arc<dyn SlackClient> {
        &self.client
    }
}

impl PlanApplier for SlackApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The Slack applier is stateless, so this delegates to the same `&self` core as the
    /// runtime bridge. The structured [`crate::error::SlackError`] is reduced to the plan crate's
    /// owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect =
            SlackEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        let affected = self
            .client
            .apply(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}

#[cfg(feature = "runtime")]
mod runtime_bridge {
    use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

    use super::SlackApplier;
    use crate::effect::SlackEffect;
    use crate::error::SlackError;

    impl SlackApplier {
        /// Whether a transient failure on this effect may be retried. A `chat.postMessage` is
        /// **never** retried (at-least-once); everything else either carries a stable target id or
        /// is naturally idempotent, so a transient failure is retry-safe.
        const fn retry_safe(effect: &SlackEffect) -> bool {
            !effect.is_at_least_once_post()
        }
    }

    impl SharedApplier for SlackApplier {
        fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
            let effect =
                SlackEffect::from_node(node).map_err(|e| EffectError::terminal(e.to_string()))?;
            let retry_safe = Self::retry_safe(&effect);
            let affected = self
                .client
                .apply(&effect)
                .map_err(|e| into_effect_error(e, retry_safe))?;
            Ok(EffectOutput::new(node.id, affected))
        }
    }

    use qfs_plan::EffectNode;

    /// Lower a [`SlackError`] into the runtime's [`EffectError`] recovery class. A transient HTTP
    /// status (5xx/429) becomes [`EffectError::retryable`] **only** when the effect was retry-safe;
    /// a non-idempotent post's transient failure, a [`SlackError::Body`] (`ok:false` is a terminal
    /// application error), and everything else are reported **terminal** so the interpreter never
    /// re-sends them (RFD §6).
    fn into_effect_error(err: SlackError, retry_safe: bool) -> EffectError {
        match &err {
            SlackError::Http { status, .. } if SlackError::is_transient_status(*status) => {
                if retry_safe {
                    EffectError::retryable(err.to_string())
                } else {
                    EffectError::terminal(err.to_string())
                }
            }
            SlackError::Transport { .. } => {
                if retry_safe {
                    EffectError::retryable(err.to_string())
                } else {
                    EffectError::terminal(err.to_string())
                }
            }
            // BodyError (ok:false), auth, decode, capability, malformed — all terminal.
            _ => EffectError::terminal(err.to_string()),
        }
    }
}
