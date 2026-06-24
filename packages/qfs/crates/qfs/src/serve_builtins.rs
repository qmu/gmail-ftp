//! Built-in self-contained read sources the `qfs serve` daemon registers (t36).
//!
//! A bare `qfs serve` registers NO external read drivers — the deployment registers the real E4
//! drivers, and an unregistered source surfaces as a structured 422 at request time (never a
//! panic). But the daemon also needs at least one ALWAYS-AVAILABLE, credential-free read source so
//! a liveness ENDPOINT serves a real well-formed body over loopback (the t36 acceptance: `qfs
//! serve` serves one ENDPOINT returning the expected JSON body). [`StatusDriver`] is that source: a
//! one-row `/status` relational table (a daemon health row) backed entirely in-process — no
//! network, no creds, no external dependency. An endpoint declared `AS FROM /status` resolves,
//! plans, scans, and encodes to a real `{"ok":...}` JSON body.

use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, Driver, Engine, NodeDesc, Path,
    PushdownProfile, Row, RowBatch, Schema, Value,
};
use qfs_exec::{ReadDriver, ReadRegistry};
use qfs_pushdown::ScanNode;

/// The mount the built-in liveness source answers under.
pub const STATUS_MOUNT: &str = "/status";

/// The `/status` row schema: `(ok: Int, service: Text)`. A minimal, stable liveness projection.
fn status_schema() -> Schema {
    Schema::new(vec![
        Column::new("ok", ColumnType::Int, false),
        Column::new("service", ColumnType::Text, false),
    ])
}

/// The one liveness row the daemon always serves.
fn status_row() -> Row {
    Row::new(vec![Value::Int(1), Value::Text("qfs".into())])
}

/// A self-contained, credential-free built-in read source: the `/status` daemon-liveness table.
/// Implements both `qfs_core::Driver` (so the engine can `describe`/plan it) and
/// `qfs_exec::ReadDriver` (so the executor can scan it). A read-only source — it advertises only
/// `SELECT`, so an endpoint over it never lowers a write plan.
#[derive(Debug, Default)]
pub struct StatusDriver;

impl StatusDriver {
    /// A fresh status driver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// A no-op applier: `/status` is read-only (it advertises no write verbs), so this is never
/// reached on a write plan; it exists only to satisfy the `Driver` contract.
#[derive(Default)]
struct NoopApplier;
impl qfs_core::PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_core::EffectNode,
    ) -> Result<qfs_core::AppliedEffect, qfs_core::ApplyError> {
        Ok(qfs_core::AppliedEffect::new(node.id, 0))
    }
}

impl Driver for StatusDriver {
    fn mount(&self) -> &str {
        STATUS_MOUNT
    }

    fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, status_schema()))
    }

    fn capabilities(&self, _path: &Path) -> Capabilities {
        // Read-only: only SELECT. A write-lowering endpoint over /status is refused by the
        // capability gate (it never even reaches the policy gate).
        Capabilities::none().select()
    }

    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }

    fn pushdown(&self) -> &PushdownProfile {
        // None: a residual WHERE/LIMIT is re-applied locally over the single returned row.
        &PushdownProfile::None
    }

    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[async_trait::async_trait]
impl ReadDriver for StatusDriver {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        Ok(RowBatch::new(status_schema(), vec![status_row()]))
    }
}

/// Register the built-in `/status` source into the serve engine + read registry (so a liveness
/// ENDPOINT `AS FROM /status` resolves, plans, scans, and encodes a real JSON body). Returns the
/// mounted engine + reads. The deployment registers its real E4 drivers ON TOP of these built-ins.
pub fn register_builtins(engine: &mut Engine, reads: &mut ReadRegistry) {
    // The describe/plan facet (the engine resolves the source's schema + pushdown from here).
    if let Err(e) = engine.mounts.register(Arc::new(StatusDriver::new())) {
        // A duplicate registration is a wiring bug, not a runtime fault; log + continue (the
        // daemon stays panic-free, RFD §6).
        tracing::warn!(target: "qfs::serve", error = %e, "could not register /status built-in");
        return;
    }
    // The read facet (the executor scans the source through this). The planner tags the scan's
    // source with the mount's leading SEGMENT (no slash) — `status`, not `/status` — matching the
    // SourceId the pushdown partitioner derives from the `/status` FROM path.
    let source_id = STATUS_MOUNT.trim_start_matches('/');
    reads.register(
        qfs_core::DriverId::new(source_id),
        Arc::new(StatusDriver::new()),
    );
}
