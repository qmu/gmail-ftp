//! Pure plan-level coordinates for a `/server/...` self-config write (RFD-0001 §6/§8).
//!
//! The server's own configuration (endpoints / triggers / jobs / views / policies /
//! webhooks) is **data** under the `/server/...` mount, managed by the same DSL the
//! server runs. A write to `/server/...` lowers to an [`EffectKind::ServerConfigWrite`]
//! effect node carrying these two **pure enums** — *which* config node and *which* write
//! op — while the config payload itself rides in the node's owned
//! [`RowBatch`](qfs_types::RowBatch) `args` (the established effect-data carrier).
//!
//! ## Why the DTO is not here (purity invariant, RFD §3)
//! `qfs-plan` is the I/O-free effect substrate; it must not depend on `qfs-server` (that
//! would be a cycle and would leak server-shaped DTOs into the pure core). So the plan
//! node carries only these vendor-free coordinates + the generic `RowBatch` payload; the
//! owned config DTOs (`EndpointDef`/`TriggerDef`/…) and the COMMIT-time apply that takes
//! the `RwLock` write guard live in `qfs-server`. Booting a config file therefore replays
//! the *same* [`EffectKind::ServerConfigWrite`] nodes a live write produces — there is no
//! privileged config loader.
//!
//! [`EffectKind::ServerConfigWrite`]: crate::EffectKind::ServerConfigWrite

use serde::Serialize;

/// Which `/server/...` config collection a self-config write targets (RFD §8). A
/// **closed, vendor-free** set mirroring the frozen `CREATE
/// ENDPOINT|TRIGGER|JOB|VIEW|MATERIALIZED VIEW|WEBHOOK|POLICY` DDL — a new backend adds
/// **zero** variants. Owned data; no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerNode {
    /// `/server/endpoints` — HTTP endpoints (`CREATE ENDPOINT`).
    Endpoints,
    /// `/server/triggers` — event triggers (`CREATE TRIGGER`).
    Triggers,
    /// `/server/jobs` — cron jobs (`CREATE JOB`).
    Jobs,
    /// `/server/views` — views, materialized or not (`CREATE [MATERIALIZED] VIEW`).
    Views,
    /// `/server/policies` — least-privilege policies (`CREATE POLICY`).
    Policies,
    /// `/server/webhooks` — inbound webhook routes (`CREATE WEBHOOK`).
    Webhooks,
}

impl ServerNode {
    /// The canonical `/server/...` collection segment, e.g. `triggers`. The single
    /// source of truth shared by path rendering and the DDL desugar so the two cannot
    /// drift.
    #[must_use]
    pub const fn segment(self) -> &'static str {
        match self {
            ServerNode::Endpoints => "endpoints",
            ServerNode::Triggers => "triggers",
            ServerNode::Jobs => "jobs",
            ServerNode::Views => "views",
            ServerNode::Policies => "policies",
            ServerNode::Webhooks => "webhooks",
        }
    }

    /// Resolve a `/server/...` collection segment back to its [`ServerNode`], if known.
    /// `materialized_views` collapses onto [`ServerNode::Views`] (a materialized view is
    /// a view row with `materialized = true`, not a separate collection).
    #[must_use]
    pub fn from_segment(seg: &str) -> Option<Self> {
        match seg {
            "endpoints" => Some(ServerNode::Endpoints),
            "triggers" => Some(ServerNode::Triggers),
            "jobs" => Some(ServerNode::Jobs),
            "views" | "materialized_views" => Some(ServerNode::Views),
            "policies" => Some(ServerNode::Policies),
            "webhooks" => Some(ServerNode::Webhooks),
            _ => None,
        }
    }

    /// The fully-qualified `/server/<segment>` mount path for this node.
    #[must_use]
    pub fn path(self) -> String {
        format!("/server/{}", self.segment())
    }
}

/// The write op of a [`EffectKind::ServerConfigWrite`](crate::EffectKind::ServerConfigWrite)
/// — a **closed** subset of the universal write verbs that `/server/...` nodes accept
/// (RFD §6). `Upsert` is modelled distinctly so a boot replay / retry converges to the
/// same `ServerState` (idempotency, RFD §6). Owned data; no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerWriteOp {
    /// `INSERT INTO /server/...` — add a config row (fails on a duplicate name).
    Insert,
    /// `UPSERT INTO /server/...` — idempotent create-or-replace (retry/replay-safe).
    Upsert,
    /// `UPDATE /server/...` — modify an existing config row.
    Update,
    /// `REMOVE /server/...` — delete a config row.
    Remove,
}

impl ServerWriteOp {
    /// A short, stable label for previews / golden snapshots.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ServerWriteOp::Insert => "INSERT",
            ServerWriteOp::Upsert => "UPSERT",
            ServerWriteOp::Update => "UPDATE",
            ServerWriteOp::Remove => "REMOVE",
        }
    }
}
