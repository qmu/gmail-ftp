//! The `/sys/*` node model: the [`SysNode`] sum type, its path↔segment mapping, the
//! single-source-of-truth [`sys_node_schema`], and the per-node [`sys_node_capabilities`].
//!
//! This is the **pure, credential-free** introspective surface (RFD-0001 §3 purity / §5). It
//! mirrors the closed-core `server_node_schema(node)` pattern: `DESCRIBE /sys/users` returns a
//! stable typed [`Schema`] with **no DB and no secrets**, so describe (and the parse-time
//! capability gate) read one source of truth that can never drift from the rows the backend
//! later scans. NOTHING here opens a connection or reads the vault.
//!
//! ## Redaction is structural (roadmap §3.2)
//! `/sys/connections` declares ONLY `driver` / `connection` / `created_at` — names + metadata.
//! There is **no column** for a secret, a nonce, or a ciphertext, so a credential cannot surface
//! through this path even by accident: the schema is the boundary.

use qfs_types::{Column, ColumnType, Schema};

/// One administrative `/sys/<node>` relation — the deployment's own state surfaced as a path
/// (roadmap §3.4 / M3). A **closed set**; a new admin view adds a variant here, never a
/// side-channel API (the one-engine constraint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysNode {
    /// `/sys/users` — the human identities (t45 `users`), READ-only metadata.
    Users,
    /// `/sys/projects` — the host's projects (t42 `projects`), READ-only metadata.
    Projects,
    /// `/sys/audit` — the hash-chained audit live tail (t76 `audit_tail`). APPEND-ONLY: read
    /// only, never `UPDATE`/`REMOVE` — every `/sys` mutation appends here, so administration
    /// observes itself.
    Audit,
    /// `/sys/connections` — the connection **registry**: names + metadata ONLY (driver +
    /// connection label + created_at), NEVER secret material (the vault is never read here).
    Connections,
    /// `/sys/policies` — the policy grants (the path façade over the policy model). The one
    /// gated WRITE in this slice: `INSERT INTO /sys/policies`.
    Policies,
    /// `/sys/metrics` — the telemetry counter live view (t77): the current process's metric
    /// counters (`name`/`kind`/`value`), READ-only. qfs EMITS metrics to the configured sink and
    /// does not store the stream (decision V); this is the bounded in-process snapshot, not a
    /// durable time series (retention is the consumer's, via the sink).
    Metrics,
    /// `/sys/settings` — the deployment SETTINGS key/value (t59): the home of the selectable AI
    /// safety mode (`key`/`value`/`updated_at`). The second gated WRITE in this surface: SELECT to
    /// review, an upsert-on-`key` `INSERT INTO /sys/settings` to set a value (super-admin only).
    Settings,
    /// `/sys/billing` — the per-team billing PLAN (t67, roadmap §3.4 / M9): the recorded tier +
    /// subscription status, surfaced as DATA (`team_id`/`tier`/`status`/`current_period_end`/
    /// `updated_at`). The third gated WRITE in this surface: SELECT to review, an upsert-on-`team_id`
    /// `INSERT INTO /sys/billing` to record/grant a tier (super-admin only). NEVER a payment secret —
    /// the provider keys + webhook signing secrets live envelope-encrypted in the vault, never here.
    Billing,
}

impl SysNode {
    /// Resolve a path segment (`users`/`projects`/`audit`/`connections`/`policies`) to its node.
    #[must_use]
    pub fn from_segment(seg: &str) -> Option<Self> {
        match seg {
            "users" => Some(Self::Users),
            "projects" => Some(Self::Projects),
            "audit" => Some(Self::Audit),
            "connections" => Some(Self::Connections),
            "policies" => Some(Self::Policies),
            "metrics" => Some(Self::Metrics),
            "settings" => Some(Self::Settings),
            "billing" => Some(Self::Billing),
            _ => None,
        }
    }

    /// The path segment naming this node (`users`, `audit`, …).
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Projects => "projects",
            Self::Audit => "audit",
            Self::Connections => "connections",
            Self::Policies => "policies",
            Self::Metrics => "metrics",
            Self::Settings => "settings",
            Self::Billing => "billing",
        }
    }

    /// The full mount-qualified path of this node (`/sys/users`, …).
    #[must_use]
    pub fn path(self) -> String {
        format!("{SYS_MOUNT}/{}", self.segment())
    }

    /// Whether this node is the append-only audit log (read-only; no `UPDATE`/`REMOVE`).
    #[must_use]
    pub fn is_append_log(self) -> bool {
        matches!(self, Self::Audit)
    }
}

/// The reserved mount point for the administration-as-a-driver (roadmap §3.4 / decision P:
/// `/sys` is a closed singleton realm).
pub const SYS_MOUNT: &str = "/sys";

/// Resolve a `/sys/...` path to its [`SysNode`], if the path names a known admin relation.
/// `/sys/audit` and `/sys/audit/<x>` both resolve to [`SysNode::Audit`]. Returns `None` for
/// `/sys` itself or an unknown segment.
#[must_use]
pub fn node_for_path(path: &str) -> Option<SysNode> {
    let rest = path
        .strip_prefix("/sys/")
        .or_else(|| path.strip_prefix("sys/"))?;
    let segment = rest.split('/').next().unwrap_or(rest);
    SysNode::from_segment(segment)
}

/// The typed [`Schema`] of a `/sys/<node>` relation — the **canonical** source of truth
/// `DESCRIBE /sys/<node>` and the backend scan both read. Pure data; no live backend, no creds.
///
/// `/sys/connections` carries ONLY name/metadata columns — there is structurally **no** column
/// for a secret (the redaction contract, roadmap §3.2).
#[must_use]
pub fn sys_node_schema(node: SysNode) -> Schema {
    let col = |name: &str, ty: ColumnType, nullable: bool| Column::new(name, ty, nullable);
    match node {
        // t45 `users`: the human identity (authentication handle), metadata only.
        SysNode::Users => Schema::new(vec![
            col("id", ColumnType::Int, false),
            col("primary_email", ColumnType::Text, false),
            col("status", ColumnType::Text, false),
            col("created_at", ColumnType::Text, true),
        ]),
        // t42 `projects`: the host's projects.
        SysNode::Projects => Schema::new(vec![
            col("id", ColumnType::Int, false),
            col("slug", ColumnType::Text, false),
            col("created_at", ColumnType::Text, true),
        ]),
        // t76 `audit_tail`: the hash-chained live tail — METADATA ONLY (actor/connection/verb/
        // path/committed/ts), the same boundary `describe` enforces. Never a secret, never a row
        // payload, so it is safe to render as a relation.
        SysNode::Audit => Schema::new(vec![
            col("seq", ColumnType::Int, false),
            col("actor", ColumnType::Text, false),
            col("connection", ColumnType::Text, false),
            col("verb", ColumnType::Text, false),
            col("path", ColumnType::Text, false),
            col("committed", ColumnType::Bool, false),
            col("ts", ColumnType::Text, false),
        ]),
        // The connection REGISTRY: names + metadata ONLY (roadmap §3.2). Reads the registry, not
        // the vault — there is no `nonce`/`ciphertext`/`secret` column here BY DESIGN.
        SysNode::Connections => Schema::new(vec![
            col("driver", ColumnType::Text, false),
            col("connection", ColumnType::Text, false),
            col("created_at", ColumnType::Text, true),
        ]),
        // The policy grants — the path façade over the policy model. `allow` is the granted verb
        // (e.g. `SELECT`); `target` is the driver/path glob the grant applies to.
        SysNode::Policies => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("allow", ColumnType::Text, true),
            col("target", ColumnType::Text, true),
            col("created_at", ColumnType::Text, true),
        ]),
        // t77 telemetry counters: the current process's metric snapshot — METADATA ONLY
        // (instrument name + kind + numeric value). No secret, no row payload; safe to render as a
        // relation, the same boundary `describe` enforces.
        SysNode::Metrics => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("kind", ColumnType::Text, false),
            col("value", ColumnType::Int, false),
        ]),
        // t59 deployment settings: a generic key/value (the safety-mode home). Metadata only — a
        // setting name + its value + when it changed; no secret, no row payload.
        SysNode::Settings => Schema::new(vec![
            col("key", ColumnType::Text, false),
            col("value", ColumnType::Text, false),
            col("updated_at", ColumnType::Text, true),
        ]),
        // t67 billing plan: the per-team recorded tier + subscription status (the gate's authority),
        // surfaced as DATA. Metadata only — a team id, the tier/status labels, the period end, when
        // it changed. There is structurally NO column a payment secret / card / provider key could
        // ride in (the redaction contract, §3.2): the provider material lives in the vault.
        SysNode::Billing => Schema::new(vec![
            col("team_id", ColumnType::Text, false),
            col("tier", ColumnType::Text, false),
            col("status", ColumnType::Text, false),
            col("current_period_end", ColumnType::Text, true),
            col("updated_at", ColumnType::Text, true),
        ]),
    }
}
