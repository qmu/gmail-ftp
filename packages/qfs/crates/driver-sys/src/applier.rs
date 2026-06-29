//! [`SysApplier`] — the `/sys` driver's apply leg (RFD-0001 §6). It lowers a write effect node
//! into the one gated System-DB mutation this slice ships: `INSERT INTO /sys/policies`. Every
//! other write is rejected here (belt-and-suspenders over the parse-time capability gate):
//! `/sys/audit` is append-only and the remaining admin views are read-only.
//!
//! The real I/O happens in the injected [`SysBackend`] (binary-side rusqlite); the applier is a
//! pure router over the owned effect node, so it is stateless and `&self`-applies through the
//! runtime's [`SharedApplier`] bridge.
//!
//! The backend appends the t76 audit row transactionally with the policy write — so the audit
//! emission is NOT duplicated by the CLI commit path's best-effort emitter (which skips `/sys`
//! legs precisely because they self-audit at the source of truth).

use qfs_plan::{AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use std::sync::Arc;

use crate::backend::{SysBackend, SysError};
use crate::schema::{node_for_path, SysNode};

/// The synchronous `/sys` apply leg. Holds the injected backend behind an `Arc` (so the leg is
/// cheap to clone and `&self`-apply). Stateless across calls.
#[derive(Clone)]
pub struct SysApplier {
    backend: Arc<dyn SysBackend>,
}

impl SysApplier {
    /// Build an applier over an injected [`SysBackend`] (the binary's System-DB implementation).
    #[must_use]
    pub fn new(backend: Arc<dyn SysBackend>) -> Self {
        Self { backend }
    }

    /// Route one effect node to the backend: resolve the `/sys` node, gate the verb, and apply.
    /// Only `INSERT INTO /sys/policies` is permitted; everything else is a structured rejection.
    fn apply_node(&self, node: &EffectNode) -> Result<u64, SysError> {
        let path = node.target.path.as_str();
        let sys_node = node_for_path(path).ok_or_else(|| SysError::UnknownNode {
            path: path.to_string(),
        })?;

        match (&node.kind, sys_node) {
            // The gated writes: a policy grant, or a deployment setting (the safety mode — t59,
            // upsert-on-`key`). Both land in the System DB + append a t76 audit row in one txn.
            (EffectKind::Insert, SysNode::Policies) => self.backend.insert_policy(&node.args),
            (EffectKind::Insert | EffectKind::Upsert, SysNode::Settings) => {
                self.backend.set_setting(&node.args)
            }
            // t67: record/grant a team's billing tier (upsert-on-`team_id`). The gate later reads
            // this plan state; the write is a /sys mutation (previewed, committed, self-audited).
            (EffectKind::Insert | EffectKind::Upsert, SysNode::Billing) => {
                self.backend.set_billing(&node.args)
            }
            // /sys/audit is append-only; the other admin views are read-only. Reject every other
            // write at the applier too (so even a hand-built plan that bypassed the parse-time
            // capability gate cannot mutate them).
            (kind, n) => Err(SysError::AppendOnly {
                node: n.segment(),
                verb: static_verb_label(kind),
            }),
        }
    }
}

/// The stable `&'static str` label for an effect kind (the structured-error field is `&'static`).
fn static_verb_label(kind: &EffectKind) -> &'static str {
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

impl SharedApplier for SysApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| EffectError::terminal(e.to_string()))?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for SysApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09). Stateless, so it delegates to
    /// the same `&self` core as [`SharedApplier::apply_shared`]; the structured [`SysError`] is
    /// reduced to the plan crate's owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_plan::{DriverId, NodeId, Target, VfsPath};
    use qfs_types::{Column, ColumnType, RowBatch, Schema, Value};
    use std::sync::Mutex;

    use qfs_types::Row;

    /// An in-memory fake backend (no DB, no creds): records the policy rows it was asked to
    /// insert, so the applier's ROUTING can be proven without the binary's rusqlite impl.
    #[derive(Default)]
    struct FakeBackend {
        inserted: Mutex<Vec<RowBatch>>,
    }

    impl SysBackend for FakeBackend {
        fn scan(&self, _node: SysNode) -> Result<RowBatch, SysError> {
            Ok(RowBatch::new(Schema::new(vec![]), vec![]))
        }
        fn insert_policy(&self, row: &RowBatch) -> Result<u64, SysError> {
            self.inserted.lock().unwrap().push(row.clone());
            Ok(1)
        }
        fn set_setting(&self, row: &RowBatch) -> Result<u64, SysError> {
            self.inserted.lock().unwrap().push(row.clone());
            Ok(1)
        }
        fn set_billing(&self, row: &RowBatch) -> Result<u64, SysError> {
            self.inserted.lock().unwrap().push(row.clone());
            Ok(1)
        }
    }

    fn policy_row() -> RowBatch {
        let schema = Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("allow", ColumnType::Text, true),
            Column::new("target", ColumnType::Text, true),
        ]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text("analysts".into()),
                Value::Text("SELECT".into()),
                Value::Text("/sql/*".into()),
            ])],
        )
    }

    fn effect(kind: EffectKind, path: &str, args: RowBatch) -> EffectNode {
        EffectNode::new(
            NodeId(0),
            kind,
            Target::new(DriverId::new("sys"), VfsPath::new(path)),
        )
        .with_args(args)
    }

    #[test]
    fn insert_into_sys_policies_routes_to_the_backend() {
        let backend = Arc::new(FakeBackend::default());
        let applier = SysApplier::new(backend.clone());
        let node = effect(EffectKind::Insert, "/sys/policies", policy_row());
        let out = applier.apply_shared(&node).expect("policy insert applies");
        assert_eq!(out.affected, 1);
        assert_eq!(
            backend.inserted.lock().unwrap().len(),
            1,
            "row reached backend"
        );
    }

    #[test]
    fn insert_into_sys_settings_routes_to_the_backend() {
        // t59: `INSERT INTO /sys/settings` (the safety-mode setter) routes to set_setting.
        let backend = Arc::new(FakeBackend::default());
        let applier = SysApplier::new(backend.clone());
        let schema = Schema::new(vec![
            Column::new("key", ColumnType::Text, false),
            Column::new("value", ColumnType::Text, false),
        ]);
        let row = RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text("safety_mode".into()),
                Value::Text("policy-only".into()),
            ])],
        );
        let node = effect(EffectKind::Insert, "/sys/settings", row);
        let out = applier
            .apply_shared(&node)
            .expect("settings upsert applies");
        assert_eq!(out.affected, 1);
        assert_eq!(
            backend.inserted.lock().unwrap().len(),
            1,
            "row reached backend"
        );
    }

    #[test]
    fn insert_into_sys_billing_routes_to_the_backend() {
        // t67: `INSERT INTO /sys/billing` (the tier recorder) routes to set_billing.
        let backend = Arc::new(FakeBackend::default());
        let applier = SysApplier::new(backend.clone());
        let schema = Schema::new(vec![
            Column::new("team_id", ColumnType::Text, false),
            Column::new("tier", ColumnType::Text, false),
            Column::new("status", ColumnType::Text, false),
        ]);
        let row = RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text("team-acme".into()),
                Value::Text("paid-team".into()),
                Value::Text("active".into()),
            ])],
        );
        let node = effect(EffectKind::Insert, "/sys/billing", row);
        let out = applier.apply_shared(&node).expect("billing upsert applies");
        assert_eq!(out.affected, 1);
        assert_eq!(
            backend.inserted.lock().unwrap().len(),
            1,
            "row reached backend"
        );
    }

    #[test]
    fn update_or_remove_on_audit_is_rejected_in_the_applier() {
        // Belt-and-suspenders over the parse-time gate: even a hand-built plan cannot mutate the
        // append-only audit log (or any read-only admin view).
        let applier = SysApplier::new(Arc::new(FakeBackend::default()));
        for (kind, path) in [
            (EffectKind::Update, "/sys/audit"),
            (EffectKind::Remove, "/sys/audit"),
            (EffectKind::Insert, "/sys/users"),
            (EffectKind::Insert, "/sys/connections"),
        ] {
            let node = effect(kind, path, RowBatch::new(Schema::new(vec![]), vec![]));
            assert!(
                applier.apply_shared(&node).is_err(),
                "{path} must reject a write in the applier"
            );
        }
    }
}
