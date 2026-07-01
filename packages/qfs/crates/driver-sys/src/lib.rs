//! `qfs-driver-sys` — the **administration driver** (RFD-0001 §5; roadmap §3.4 / M3, t53).
//!
//! "Administration is also everything-is-a-path." [`SysDriver`] exposes the deployment's own
//! state — `/sys/users`, `/sys/projects`, `/sys/audit`, `/sys/connections`, `/sys/policies`,
//! `/sys/metrics` — as
//! ordinary qfs relations backed by the System DB (t42), so a super-admin does every
//! administrative action as a qfs statement (`/sys/audit |> WHERE …`, gated
//! `INSERT INTO /sys/policies VALUES (…)`) from the CLI, MCP, or dashboard — **one engine, three
//! faces**.
//!
//! ## The same split as the `/server` self-config driver
//! [`SysDriver`] is the EXACT analogue of `qfs-server`'s `ServerDriver`: its **introspective**
//! half ([`Driver::describe`]/[`Driver::capabilities`]/[`Driver::pushdown`]) is **pure** — a
//! stable, credential-free schema (see [`sys_node_schema`]) with NO DB and NO secrets — and its
//! `applier()` is a [`NoopApplier`]. The real mutation lands in a runtime [`SysApplier`] over the
//! injected [`SysBackend`] (binary-side rusqlite over the System DB), bridged via
//! [`sys_apply_driver`]. The crate therefore stays tokio-free and DB-free; the binary leaf is the
//! one place that opens a real DB path (decision F).
//!
//! ## Safety floor (roadmap §3.2 / §4.6)
//! - `/sys/connections` projects connection **names + metadata only** — never secret material
//!   (the schema has no secret column; the implementor reads the registry, not the vault).
//! - `/sys/audit` is **append-only** — `SELECT` only; `UPDATE`/`REMOVE` are rejected at the
//!   parse-time capability gate AND in the applier. Every `/sys` mutation appends an audit row.
//! - `/sys/*` writes are high-privilege: gated by the SAME default-deny policy engine as any
//!   other driver (the path is the authorization subject), and — until the super-admin vs.
//!   project-admin split is settled — wired loopback super-admin only (flagged, not baked in).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
mod backend;
mod schema;

use std::sync::Arc;

use qfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::SysApplier;
pub use backend::{SysBackend, SysError};
pub use schema::{node_for_path, sys_node_schema, SysNode, SYS_MOUNT};

/// The administration driver (roadmap §3.4). Pure introspection only — it owns NO state and NO
/// backend (the read source + the mutation applier are injected from the binary). Construct with
/// [`SysDriver::new`].
pub struct SysDriver {
    // The admin registry is the System DB read in-engine; it pushes nothing down (honest
    // declaration, RFD §6). A bounded live tail / small tables — filtering is the engine's work.
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl Default for SysDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SysDriver {
    /// Construct the (pure) administration driver.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pushdown: PushdownProfile::None,
            procs: Vec::new(),
        }
    }
}

/// The per-node capability set (RFD §5). Every admin relation is readable; only `/sys/policies`
/// accepts the one gated `INSERT`. `/sys/audit` is append-only (read-only here — it is *emitted*,
/// never user-inserted), and the remaining views are read-only. Single source of truth shared by
/// [`Driver::capabilities`] and the parse-time verb gate.
#[must_use]
pub fn sys_node_capabilities(node: SysNode) -> Capabilities {
    match node {
        // The gated write surfaces: SELECT to review, INSERT to grant a policy / set a setting
        // (`/sys/settings` upserts on `key`, e.g. the safety mode — t59) / record a billing tier
        // (`/sys/billing` upserts on `team_id` — t67).
        SysNode::Policies | SysNode::Settings | SysNode::Billing => {
            Capabilities::from_verbs(&[Verb::Select, Verb::Insert])
        }
        // The defined-path binding registry (t100020, the CONNECT model): SELECT to review, INSERT
        // to bind/re-bind (`CONNECT` — upsert on `path`), REMOVE to disconnect (`DISCONNECT`). The
        // ONE `/sys` node that accepts REMOVE — a defined path is a user-owned mount the user may
        // withdraw, unlike the deployment's own audit/policy state.
        SysNode::Paths => Capabilities::from_verbs(&[Verb::Select, Verb::Insert, Verb::Remove]),
        // Read-only admin views (audit is append-only: emitted, never user-written; no
        // UPDATE/REMOVE — the rejection the t53 acceptance test pins).
        SysNode::Users
        | SysNode::Projects
        | SysNode::Audit
        | SysNode::Connections
        | SysNode::Metrics => Capabilities::from_verbs(&[Verb::Select]),
    }
}

impl Driver for SysDriver {
    fn mount(&self) -> &str {
        SYS_MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Pure: returns static schema data; never touches a DB or a credential.
        let node =
            node_for_path(path.as_str()).ok_or_else(|| qfs_driver::CfsError::UnsupportedVerb {
                path: path.as_str().to_string(),
                verb: "DESCRIBE",
                supported: Vec::new(),
            })?;
        // /sys/audit is the append-log archetype; the rest are relational admin tables.
        let archetype = if node.is_append_log() {
            Archetype::AppendLog
        } else {
            Archetype::RelationalTable
        };
        Ok(NodeDesc::new(archetype, sys_node_schema(node)))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        match node_for_path(path.as_str()) {
            Some(node) => sys_node_capabilities(node),
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
        // Like ServerDriver: the real /sys apply path is the runtime's SysApplier (which holds
        // the injected SysBackend and writes the System DB transactionally). The introspective
        // driver does not own that impure seam, so this is a no-op to satisfy the trait.
        &NoopApplier
    }
}

/// A no-op applier for the `Driver::applier()` contract slot (mirrors `ServerDriver`'s). The real
/// `/sys` apply path is the runtime [`SysApplier`]; this exists only so `SysDriver` satisfies the
/// introspective trait without pretending to own the impure seam. It touches no state.
struct NoopApplier;

impl PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_plan::EffectNode,
    ) -> Result<qfs_plan::AppliedEffect, qfs_plan::ApplyError> {
        Ok(qfs_plan::AppliedEffect::new(node.id, 0))
    }
}

/// Wrap a [`SysApplier`] in the runtime [`PlanApplierBridge`], yielding the async `ApplyDriver`
/// ready to `register` into a `DriverRegistry` under the driver id `sys`. A plan routed to `/sys`
/// then executes through the t10 interpreter, which dispatches the policy-grant effect to this
/// bridge (one System-DB transaction per apply).
#[must_use]
pub fn sys_apply_driver(applier: &SysApplier) -> PlanApplierBridge<SysApplier> {
    PlanApplierBridge::new(Arc::new(applier.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_driver::{check_capability, CfsError};

    /// G3 — the purity proof for the introspective half (the t53 analogue of
    /// `fixture_driver_introspection_is_pure`). `DESCRIBE /sys/users` resolves to a stable typed
    /// schema with NO DB and NO creds: the driver owns no backend, and none of these methods take
    /// `&mut self`, return a future, or perform I/O — so a no-I/O round-trip IS the proof.
    #[test]
    fn describe_sys_users_is_pure_no_db_no_creds() {
        let d = SysDriver::new();
        assert_eq!(d.mount(), "/sys");

        let users = d.describe(&Path::new("/sys/users")).unwrap();
        assert_eq!(users.archetype, Archetype::RelationalTable);
        assert!(users.schema.column("primary_email").is_some());
        // The schema carries NO secret/credential column — structurally cred-free.
        assert!(users.schema.column("password_hash").is_none());

        // /sys/audit is the append-log archetype.
        let audit = d.describe(&Path::new("/sys/audit")).unwrap();
        assert_eq!(audit.archetype, Archetype::AppendLog);

        // An unknown /sys segment is not describable (no panic).
        assert!(d.describe(&Path::new("/sys/nope")).is_err());
    }

    /// The redaction contract is structural: `/sys/connections` declares ONLY name/metadata
    /// columns — there is NO column a secret/ciphertext/nonce could ride in.
    #[test]
    fn connections_schema_has_no_secret_column() {
        let schema = sys_node_schema(SysNode::Connections);
        for forbidden in [
            "nonce",
            "ciphertext",
            "secret",
            "password_hash",
            "wrapped_dek",
        ] {
            assert!(
                schema.column(forbidden).is_none(),
                "/sys/connections must never expose `{forbidden}`"
            );
        }
        assert!(schema.column("driver").is_some());
        assert!(schema.column("connection").is_some());
    }

    /// Capability golden gate: `/sys/audit` is append-only — `UPDATE`/`REMOVE` are rejected at the
    /// parse-time gate with a structured error, while `SELECT` passes. `/sys/policies` admits the
    /// one gated `INSERT`.
    #[test]
    fn audit_rejects_update_and_remove_policies_allows_insert() {
        let d = SysDriver::new();
        let audit = Path::new("/sys/audit");
        for verb in [Verb::Update, Verb::Remove] {
            let err = check_capability(&d, &audit, verb).unwrap_err();
            assert!(
                matches!(err, CfsError::UnsupportedVerb { .. }),
                "/sys/audit must reject {} structurally",
                verb.label()
            );
        }
        assert!(check_capability(&d, &audit, Verb::Select).is_ok());

        let policies = Path::new("/sys/policies");
        assert!(check_capability(&d, &policies, Verb::Select).is_ok());
        assert!(check_capability(&d, &policies, Verb::Insert).is_ok());
        // The slice ships exactly ONE write verb on policies — no UPDATE/REMOVE yet.
        assert!(check_capability(&d, &policies, Verb::Remove).is_err());
    }

    /// The driver is object-safe (`Arc<dyn Driver>`) — the registries store trait objects (G2).
    #[test]
    fn sys_driver_is_object_safe() {
        let d: Arc<dyn Driver> = Arc::new(SysDriver::new());
        assert_eq!(d.mount(), "/sys");
        let _seam: &dyn PlanApplier = d.applier();
    }
}
