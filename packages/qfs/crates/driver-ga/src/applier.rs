//! [`GaApplier`] — the Google Analytics driver's apply leg, which is **read-only by
//! construction** (RFD-0001 §5/§10). GA4 is a query source, never a mutate target: there is no
//! GA write effect to apply, so this applier **rejects every effect** routed to it with a
//! structured [`GaError::ReadOnly`] (the capability gate rejects the verb at parse time first;
//! this is the belt-and-suspenders enforcement at the apply boundary, so even a hand-built plan
//! cannot mutate GA).
//!
//! Report rows (`SELECT`) are produced through the pure **read path** ([`crate::report`]), not
//! through this applier — the applier seam exists solely because the [`qfs_driver::Driver`]
//! contract requires one. It is stateless and holds no token (auth is wholly behind the
//! [`GaClient`](crate::client::GaClient), which the read path uses).

use std::sync::Arc;

use qfs_plan::{AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use crate::client::GaClient;
use crate::error::GaError;

/// The Google Analytics apply leg. Holds the [`GaClient`] behind an `Arc` (so the read path can
/// share it and the leg is cheap to clone for the runtime bridge), but applies **no** writes — GA
/// is read-only, so every apply is rejected with a structured read-only error.
#[derive(Clone)]
pub struct GaApplier {
    #[allow(dead_code)]
    client: Arc<dyn GaClient>,
}

impl GaApplier {
    /// Build an applier over `client`. The client is carried for symmetry with the other Google
    /// drivers (and so the bridge can be constructed), but no write op ever reaches it.
    #[must_use]
    pub fn new(client: Arc<dyn GaClient>) -> Self {
        Self { client }
    }

    /// The stable verb label for an effect kind — used to build the structured read-only error.
    fn verb_label(kind: &EffectKind) -> &'static str {
        match kind {
            EffectKind::Read => "READ",
            EffectKind::List => "LIST",
            EffectKind::Insert => "INSERT",
            EffectKind::Upsert => "UPSERT",
            EffectKind::Update => "UPDATE",
            EffectKind::Remove => "REMOVE",
            EffectKind::Call(_) => "CALL",
            _ => "WRITE",
        }
    }

    /// Reject any effect against `/ga` as a read-only violation. Centralized so both the
    /// [`SharedApplier`] and [`PlanApplier`] legs agree.
    fn reject(node: &EffectNode) -> GaError {
        GaError::ReadOnly {
            path: node.target.path.as_str().to_string(),
            verb: Self::verb_label(&node.kind),
        }
    }
}

impl SharedApplier for GaApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        Err(Self::reject(node).into())
    }
}

impl PlanApplier for GaApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09). GA is read-only, so this
    /// rejects every effect with the structured read-only error reduced to the plan crate's owned
    /// `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        Err(ApplyError::new(node.id, Self::reject(node).to_string()))
    }
}
