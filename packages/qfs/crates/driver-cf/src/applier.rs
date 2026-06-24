//! [`CfApplier`] — the Cloudflare driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::CfDriver`] hands back via `applier()`, and the
//! [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`.
//!
//! Stateless across the call: it holds the [`CfRegistry`] (backends behind `Arc`s) and performs
//! fresh Cloudflare API I/O on every call — so it implements `SharedApplier` (`&self` apply), the
//! statelessness contract the bridge requires.
//!
//! ## D1 atomicity (RFD §6)
//! A D1 write applies through the [`CfBackend::d1_batch`](crate::backend::CfBackend::d1_batch)
//! endpoint — D1 has no interactive BEGIN/COMMIT, so one batch IS one atomic transaction. The
//! lowered [`DmlOp`] is rendered to **injection-safe** parameterized SQL by the reused t17 sqlite
//! emitter, and the bound values ride as a **structured array** to the backend — never
//! interpolated.
//!
//! ## KV / Queue idempotency (RFD §6)
//! A KV put is retry-safe (create-or-replace by key); a KV delete is idempotent (re-deletable).
//! A queue send carries an idempotency key so an at-least-once retry does not double-append — but
//! a send is flagged `irreversible` upstream, so the runtime never auto-retries it anyway.
//!
//! ## Secret safety
//! No API token or bound parameter VALUE is ever logged or placed in a [`CfError`]; the token is
//! wholly behind the backend (written into a redacted header), never here.

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};
use qfs_sql_core::{render_dml, Dialect};

use crate::effect::CfEffect;
use crate::error::CfError;
use crate::registry::CfRegistry;

/// The synchronous Cloudflare apply leg. Holds the [`CfRegistry`] (backends behind `Arc`s) so the
/// leg is cheap to clone for the runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct CfApplier {
    registry: CfRegistry,
}

impl CfApplier {
    /// Build an applier over a Cloudflare resource registry.
    #[must_use]
    pub fn new(registry: CfRegistry) -> Self {
        Self { registry }
    }

    /// Apply one effect node: decode it to a [`CfEffect`] (resolving the D1 catalog from the
    /// registry for the DML lowering), then dispatch to the addressed backend. Returns the
    /// affected count.
    fn apply_node(&self, node: &EffectNode) -> Result<u64, CfError> {
        // Decode using the registry's D1 catalog for the (reused t17) DML lowering. The closure
        // borrows the catalog; the decoded `CfEffect` owns everything it needs past this point.
        let effect = CfEffect::from_node(node, |db, table| {
            let handle = self.registry.d1(db)?;
            handle.table(table, node.target.path.as_str())
        })?;
        self.apply_effect(&effect)
    }

    /// Apply one decoded [`CfEffect`] against the addressed backend. The single place Cloudflare
    /// API I/O happens.
    fn apply_effect(&self, effect: &CfEffect) -> Result<u64, CfError> {
        match effect {
            CfEffect::D1Dml { db, op } => {
                // Render the t17 DmlOp to injection-safe parameterized SQLite SQL: identifiers
                // quoted, every value a `?` placeholder, values returned as a structured params
                // array. Apply in ONE D1 /batch = one atomic transaction.
                let (sql, params) = render_dml(Dialect::Sqlite, op);
                let backend = self.registry.d1(db)?.backend().clone();
                backend.d1_batch(db, &[(sql, params)])
            }
            CfEffect::KvPut { ns, entry } => {
                let backend = self.registry.kv(ns)?.clone();
                backend.kv_put(ns, entry)?;
                Ok(1)
            }
            CfEffect::KvDelete { ns, key } => {
                let backend = self.registry.kv(ns)?.clone();
                backend.kv_delete(ns, key)?;
                Ok(1)
            }
            CfEffect::QueueSend {
                queue,
                body,
                idempotency_key,
            } => {
                let backend = self.registry.queue(queue)?.clone();
                backend.queue_send(queue, body, idempotency_key)?;
                Ok(1)
            }
        }
    }

    /// Borrow the registry (e.g. for the read path: SELECT/list/tail go through the driver, not
    /// the applier).
    #[must_use]
    pub fn registry(&self) -> &CfRegistry {
        &self.registry
    }
}

impl SharedApplier for CfApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self.apply_node(node)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for CfApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The CF applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`CfError`] is reduced to the plan crate's
    /// owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
