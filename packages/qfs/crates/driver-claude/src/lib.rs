//! `qfs-driver-claude` — the **AI-sessions driver** (RFD-0001 §5; roadmap §3.3 / M7, t64).
//!
//! Each machine's Claude Code sessions become queryable qfs paths under the host realm.
//! [`ClaudeDriver`] exposes two nodes: `/claude/sessions` (READ — what each agent is doing) and
//! `/claude/sessions/<id>/instructions` (APPEND — steer a running agent). Reached BARE
//! (`/claude/...`, the `/me` realm) or under the host realm (`/hosts/<host>/claude/...`, decision
//! P / §1.3), `peel_scope` strips the realm and routes the `/claude/...` service path here; the
//! cross-machine `<host>` hop rides the t63 tunnel and re-checks `POLICY` at the destination (a
//! DOCUMENTED SEAM, fail-closed by default).
//!
//! ## Decision K — qfs NEVER hosts or calls an LLM (critical)
//! A `/claude` path is NOT qfs calling the Claude API. `/claude/sessions` is a **path façade over
//! session metadata**; `.../instructions` is an **append-log** the agent reads. The model runs
//! ELSEWHERE; qfs only exposes and steers the session surface. This crate has **no inference
//! dependency** and calls no model API.
//!
//! ## The same split as the `/sys` administration driver
//! [`ClaudeDriver`] mirrors `qfs-driver-sys`'s `SysDriver`: its **introspective** half
//! ([`Driver::describe`]/[`Driver::capabilities`]/[`Driver::pushdown`]) is **pure** — a stable,
//! credential-free schema (see [`claude_node_schema`]) with NO session source and NO secrets — and
//! its `applier()` is a [`NoopApplier`]. The real read + the gated append land in a runtime
//! [`ClaudeApplier`] over the injected [`SessionSource`] (binary-side, on-disk), bridged via
//! [`claude_apply_driver`]. The crate therefore stays tokio-free and I/O-free (wasm-buildable; the
//! purity proof [`tests::describe_sessions_is_pure_no_source_no_creds`] stays green by
//! construction), and the binary leaf is the one place that opens a real session-state path.
//!
//! ## Safety floor (roadmap §3.2 / §4.6)
//! - `/claude/sessions` is a **pure read** (describe/preview touch nothing); its schema carries no
//!   token/key/transcript column — a credential cannot surface through a path.
//! - Steering is an `INSERT` (a **reversible append**) that commits explicitly. `UPDATE`/`REMOVE`
//!   are rejected at the parse-time capability gate AND in the applier; "stop the agent", if ever
//!   added, would be an irreversible `Remove` (extra acknowledgement), never a silent reversible op.
//! - The live agent-runtime connection is **fail-closed / opt-in**: with no session source
//!   configured the binary registers no applier, so a `/claude` commit fails closed (no driver).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
mod backend;
mod schema;

use std::sync::Arc;

use qfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::ClaudeApplier;
pub use backend::{ClaudeError, SessionSource};
pub use schema::{
    claude_node_schema, instruction_session, node_for_path, ClaudeNode, CLAUDE_MOUNT,
};

/// The AI-sessions driver (roadmap §3.3). Pure introspection only — it owns NO state and NO
/// session source (the read source + the append applier are injected from the binary). Construct
/// with [`ClaudeDriver::new`].
pub struct ClaudeDriver {
    // The session surface is a bounded live view + a small append-log read in-engine; it pushes
    // nothing down (honest declaration, RFD §6) — filtering (`WHERE status='running'`) is the
    // engine's work.
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeDriver {
    /// Construct the (pure) AI-sessions driver.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pushdown: PushdownProfile::None,
            procs: Vec::new(),
        }
    }
}

/// The per-node capability set (RFD §5). `/claude/sessions` is READ-ONLY (`SELECT`); the
/// instructions append-log accepts `SELECT` + the one gated reversible `INSERT` (no `UPDATE`/
/// `REMOVE` — steering appends, it never silently removes). Single source of truth shared by
/// [`Driver::capabilities`] and the parse-time verb gate.
#[must_use]
pub fn claude_node_capabilities(node: ClaudeNode) -> Capabilities {
    match node {
        // The sessions relation: read-only metadata — what an agent is doing.
        ClaudeNode::Sessions => Capabilities::from_verbs(&[Verb::Select]),
        // The instructions append-log: SELECT to read the log, a single reversible INSERT to steer.
        ClaudeNode::Instructions => Capabilities::from_verbs(&[Verb::Select, Verb::Insert]),
    }
}

impl Driver for ClaudeDriver {
    fn mount(&self) -> &str {
        CLAUDE_MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Pure: returns static schema data; never reads a session file, opens a socket, or calls a
        // model (decision K).
        let node =
            node_for_path(path.as_str()).ok_or_else(|| qfs_driver::CfsError::UnsupportedVerb {
                path: path.as_str().to_string(),
                verb: "DESCRIBE",
                supported: Vec::new(),
            })?;
        // /claude/sessions/<id>/instructions is the append-log archetype; /claude/sessions is the
        // relational sessions table.
        let archetype = if node.is_append_log() {
            Archetype::AppendLog
        } else {
            Archetype::RelationalTable
        };
        Ok(NodeDesc::new(archetype, claude_node_schema(node)))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        match node_for_path(path.as_str()) {
            Some(node) => claude_node_capabilities(node),
            None => Capabilities::none(),
        }
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn applier(&self) -> &dyn PlanApplier {
        // Like SysDriver: the real /claude apply path is the runtime's ClaudeApplier (which holds
        // the injected SessionSource and appends the instruction). The introspective driver does
        // not own that impure seam, so this is a no-op to satisfy the trait — it touches no state
        // and makes no model call.
        &NoopApplier
    }
}

/// A no-op applier for the `Driver::applier()` contract slot (mirrors `SysDriver`'s). The real
/// `/claude` apply path is the runtime [`ClaudeApplier`]; this exists only so `ClaudeDriver`
/// satisfies the introspective trait without pretending to own the impure seam.
struct NoopApplier;

impl PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_plan::EffectNode,
    ) -> Result<qfs_plan::AppliedEffect, qfs_plan::ApplyError> {
        Ok(qfs_plan::AppliedEffect::new(node.id, 0))
    }
}

/// Wrap a [`ClaudeApplier`] in the runtime [`PlanApplierBridge`], yielding the async `ApplyDriver`
/// ready to `register` into a `DriverRegistry` under the driver id `claude`. A plan routed to
/// `/claude` then executes through the t10 interpreter, which dispatches the instruction-append
/// effect to this bridge (one append per apply).
#[must_use]
pub fn claude_apply_driver(applier: &ClaudeApplier) -> PlanApplierBridge<ClaudeApplier> {
    PlanApplierBridge::new(Arc::new(applier.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_driver::{check_capability, CfsError};
    use qfs_types::{Row, RowBatch, Value};

    /// G3 — the purity proof for the introspective half (the t64 analogue of
    /// `fixture_driver_introspection_is_pure`). `DESCRIBE /claude/sessions` resolves to a stable
    /// typed schema with NO session source and NO creds: the driver owns no backend, and none of
    /// these methods take `&mut self`, return a future, or perform I/O — so a no-I/O round-trip IS
    /// the proof. NOTHING here calls a model (decision K).
    #[test]
    fn describe_sessions_is_pure_no_source_no_creds() {
        let d = ClaudeDriver::new();
        assert_eq!(d.mount(), "/claude");

        let sessions = d.describe(&Path::new("/claude/sessions")).unwrap();
        assert_eq!(sessions.archetype, Archetype::RelationalTable);
        assert!(sessions.schema.column("status").is_some());
        assert!(sessions.schema.column("task").is_some());
        // The schema carries NO secret/credential/transcript column — structurally cred-free.
        assert!(sessions.schema.column("token").is_none());
        assert!(sessions.schema.column("transcript").is_none());

        // The per-session instructions log is the append-log archetype.
        let instr = d
            .describe(&Path::new("/claude/sessions/current/instructions"))
            .unwrap();
        assert_eq!(instr.archetype, Archetype::AppendLog);

        // The mount itself / an unknown segment is not describable (no panic).
        assert!(d.describe(&Path::new("/claude")).is_err());
        assert!(d.describe(&Path::new("/claude/nope")).is_err());
    }

    /// Capability golden gate: `/claude/sessions` is READ-ONLY — `INSERT`/`UPDATE`/`REMOVE` are
    /// rejected at the parse-time gate with a structured error, while `SELECT` passes. The
    /// instructions log admits the one gated `INSERT` (a reversible steer) but NOT `UPDATE`/`REMOVE`.
    #[test]
    fn sessions_read_only_instructions_append_only() {
        let d = ClaudeDriver::new();
        let sessions = Path::new("/claude/sessions");
        assert!(check_capability(&d, &sessions, Verb::Select).is_ok());
        for verb in [Verb::Insert, Verb::Update, Verb::Remove] {
            let err = check_capability(&d, &sessions, verb).unwrap_err();
            assert!(
                matches!(err, CfsError::UnsupportedVerb { .. }),
                "/claude/sessions must reject {} structurally",
                verb.label()
            );
        }

        let instr = Path::new("/claude/sessions/current/instructions");
        assert!(check_capability(&d, &instr, Verb::Select).is_ok());
        assert!(check_capability(&d, &instr, Verb::Insert).is_ok());
        // Steering appends; it is never a silent reversible UPDATE/REMOVE (the safety floor).
        assert!(check_capability(&d, &instr, Verb::Update).is_err());
        assert!(check_capability(&d, &instr, Verb::Remove).is_err());
    }

    /// Reading session metadata from a fixture (in-memory) source works — proving the read facet
    /// against a hermetic backend without any agent runtime, disk, or model (decision K).
    #[test]
    fn read_session_metadata_from_a_fixture_source_works() {
        struct FixtureSource;
        impl SessionSource for FixtureSource {
            fn scan_sessions(&self) -> Result<RowBatch, ClaudeError> {
                Ok(RowBatch::new(
                    claude_node_schema(ClaudeNode::Sessions),
                    vec![Row::new(vec![
                        Value::Text("s-1".into()),
                        Value::Text("write the t64 driver".into()),
                        Value::Text("running".into()),
                        Value::Text("3/5".into()),
                        Value::Text("scanning crates/driver".into()),
                    ])],
                ))
            }
            fn scan_instructions(&self, _session: &str) -> Result<RowBatch, ClaudeError> {
                Ok(RowBatch::new(
                    claude_node_schema(ClaudeNode::Instructions),
                    vec![],
                ))
            }
            fn append_instruction(
                &self,
                _session: &str,
                _row: &RowBatch,
            ) -> Result<u64, ClaudeError> {
                Ok(1)
            }
        }

        let source = FixtureSource;
        let batch = source.scan_sessions().unwrap();
        assert_eq!(batch.rows.len(), 1);
        // The schema the fixture rows conform to is exactly the one `describe` reports (no drift).
        let described = ClaudeDriver::new()
            .describe(&Path::new("/claude/sessions"))
            .unwrap();
        assert_eq!(batch.schema, described.schema);
        // The status is readable as metadata — what `WHERE status='running'` filters on.
        let status_idx = batch
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == "status")
            .unwrap();
        assert!(matches!(&batch.rows[0].values[status_idx], Value::Text(s) if s == "running"));
    }

    /// The driver is object-safe (`Arc<dyn Driver>`) — the registries store trait objects (G2).
    #[test]
    fn claude_driver_is_object_safe() {
        let d: Arc<dyn Driver> = Arc::new(ClaudeDriver::new());
        assert_eq!(d.mount(), "/claude");
        let _seam: &dyn PlanApplier = d.applier();
    }
}
