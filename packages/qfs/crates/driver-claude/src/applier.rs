//! [`ClaudeApplier`] — the `/claude` driver's apply leg (RFD-0001 §6). It lowers a write effect
//! node into the one gated mutation this driver ships: `INSERT INTO /claude/sessions/<id>/
//! instructions` (a REVERSIBLE append that steers a running agent). Every other write is rejected
//! here (belt-and-suspenders over the parse-time capability gate): `/claude/sessions` is read-only
//! and the instructions log is append-only (no `UPDATE`/`REMOVE`).
//!
//! The real I/O happens in the injected [`SessionSource`] (binary-side, on-disk); the applier is a
//! pure router over the owned effect node, so it is stateless and `&self`-applies through the
//! runtime's [`SharedApplier`] bridge.
//!
//! ## Decision K / safety floor
//! Steering is an `Insert` (a reversible append) — it adds a message to the agent's instruction
//! log, it never removes session state. "Stop the agent", if ever added, would be a separate
//! `EffectKind::Remove` (irreversible → extra acknowledgement), NEVER a silent reversible op. This
//! applier therefore services ONLY `Insert` on the instructions node; it hosts no LLM call.

use qfs_plan::{AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use std::sync::Arc;

use crate::backend::{ClaudeError, SessionSource};
use crate::schema::{instruction_session, node_for_path, ClaudeNode};

/// The synchronous `/claude` apply leg. Holds the injected session source behind an `Arc` (so the
/// leg is cheap to clone and `&self`-apply). Stateless across calls.
#[derive(Clone)]
pub struct ClaudeApplier {
    source: Arc<dyn SessionSource>,
}

impl ClaudeApplier {
    /// Build an applier over an injected [`SessionSource`] (the binary's on-disk implementation).
    #[must_use]
    pub fn new(source: Arc<dyn SessionSource>) -> Self {
        Self { source }
    }

    /// Route one effect node to the source: resolve the `/claude` node, gate the verb, and apply.
    /// Only `INSERT INTO /claude/sessions/<id>/instructions` is permitted; everything else is a
    /// structured rejection (so even a hand-built plan that bypassed the parse-time gate cannot
    /// mutate the read-only sessions relation or `UPDATE`/`REMOVE` the append-log).
    fn apply_node(&self, node: &EffectNode) -> Result<u64, ClaudeError> {
        let path = node.target.path.as_str();
        let claude_node = node_for_path(path).ok_or_else(|| ClaudeError::UnknownNode {
            path: path.to_string(),
        })?;

        match (&node.kind, claude_node) {
            // The one gated write: append a steering instruction to a session's log (reversible).
            (EffectKind::Insert, ClaudeNode::Instructions) => {
                let session =
                    instruction_session(path).ok_or_else(|| ClaudeError::UnknownNode {
                        path: path.to_string(),
                    })?;
                self.source.append_instruction(&session, &node.args)
            }
            // Everything else: read-only sessions, or UPDATE/REMOVE on the append-log. Reject in
            // the applier too (defence in depth over the parse-time capability gate).
            (kind, n) => Err(ClaudeError::Unsupported {
                node: n.label(),
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

impl SharedApplier for ClaudeApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| EffectError::terminal(e.to_string()))?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for ClaudeApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09). Stateless, so it delegates to
    /// the same `&self` core as [`SharedApplier::apply_shared`]; the structured [`ClaudeError`] is
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
    use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};
    use std::sync::Mutex;

    use crate::schema::claude_node_schema;

    /// An in-memory fake source (no disk, no creds, no LLM): records the (session, instruction)
    /// pairs it was asked to append, so the applier's ROUTING can be proven without the binary's
    /// on-disk impl.
    #[derive(Default)]
    struct FakeSource {
        appended: Mutex<Vec<(String, RowBatch)>>,
    }

    impl SessionSource for FakeSource {
        fn scan_sessions(&self) -> Result<RowBatch, ClaudeError> {
            Ok(RowBatch::new(
                claude_node_schema(ClaudeNode::Sessions),
                vec![],
            ))
        }
        fn scan_instructions(&self, _session: &str) -> Result<RowBatch, ClaudeError> {
            Ok(RowBatch::new(
                claude_node_schema(ClaudeNode::Instructions),
                vec![],
            ))
        }
        fn append_instruction(&self, session: &str, row: &RowBatch) -> Result<u64, ClaudeError> {
            self.appended
                .lock()
                .unwrap()
                .push((session.to_string(), row.clone()));
            Ok(1)
        }
    }

    fn instruction_row() -> RowBatch {
        let schema = Schema::new(vec![Column::new("instruction", ColumnType::Text, false)]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![Value::Text(
                "focus on the failing test".into(),
            )])],
        )
    }

    fn effect(kind: EffectKind, path: &str, args: RowBatch) -> EffectNode {
        EffectNode::new(
            NodeId(0),
            kind,
            Target::new(DriverId::new("claude"), VfsPath::new(path)),
        )
        .with_args(args)
    }

    #[test]
    fn insert_into_instructions_routes_to_the_source() {
        let source = Arc::new(FakeSource::default());
        let applier = ClaudeApplier::new(source.clone());
        let node = effect(
            EffectKind::Insert,
            "/claude/sessions/current/instructions",
            instruction_row(),
        );
        let out = applier
            .apply_shared(&node)
            .expect("instruction append applies");
        assert_eq!(out.affected, 1);
        let appended = source.appended.lock().unwrap();
        assert_eq!(appended.len(), 1, "row reached the source");
        assert_eq!(appended[0].0, "current", "the session id was routed");
    }

    #[test]
    fn write_to_read_only_sessions_is_rejected_in_the_applier() {
        // Belt-and-suspenders over the parse-time gate: even a hand-built plan cannot write the
        // read-only sessions relation.
        let applier = ClaudeApplier::new(Arc::new(FakeSource::default()));
        let node = effect(
            EffectKind::Insert,
            "/claude/sessions",
            RowBatch::new(Schema::new(vec![]), vec![]),
        );
        assert!(applier.apply_shared(&node).is_err());
    }

    #[test]
    fn update_or_remove_on_instructions_is_rejected_in_the_applier() {
        // The append-log is append-only: "stop the agent" is NOT a silent reversible op. A hand-built
        // UPDATE/REMOVE is rejected in the applier (no source mutation happens).
        let applier = ClaudeApplier::new(Arc::new(FakeSource::default()));
        for kind in [EffectKind::Update, EffectKind::Remove] {
            let node = effect(
                kind,
                "/claude/sessions/current/instructions",
                RowBatch::new(Schema::new(vec![]), vec![]),
            );
            assert!(
                applier.apply_shared(&node).is_err(),
                "instructions log must reject a non-append write in the applier"
            );
        }
    }
}
