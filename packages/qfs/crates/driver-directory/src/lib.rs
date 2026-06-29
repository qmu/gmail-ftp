//! `qfs-driver-directory` — the **identity-directory driver** (RFD-0001 §5; roadmap §1.2
//! decision I / M5, t58).
//!
//! "Everything is a path" extends to who you are: [`DirectoryDriver`] exposes an external identity
//! directory — LDAP / Active Directory / Entra ID / Google Workspace — as ordinary qfs relations
//! under `/directories/<provider>/...`:
//!
//! - `/directories/<provider>/groups` — the directory's groups/teams,
//! - `/directories/<provider>/users` — its user identities (metadata only),
//! - `/directories/<provider>/memberships` — the flat `(user, group)` join.
//!
//! so the t57 `member_of('/directories/<provider>/groups/<g>')` policy predicate resolves against a
//! **real directory** ("drive one [the policy] from the other [the directory]", decision I). This
//! driver supplies the data; t57 owns the predicate and keeps `evaluate` pure (membership resolved
//! into the [`DecisionContext`](../qfs_server) up front).
//!
//! ## The same split as the `/sys` self-config driver
//! [`DirectoryDriver`] is the analogue of `qfs-driver-sys`'s `SysDriver`: its **introspective**
//! half ([`Driver::describe`]/[`Driver::capabilities`]/[`Driver::pushdown`]) is **pure** — a stable,
//! credential-free schema (see [`directory_relation_schema`]) with NO directory connection and NO
//! secrets — and its `applier()` is a [`NoopApplier`]. READ-FIRST: there is no live write leg, so
//! this crate carries NO `qfs-runtime` dependency and stays wasm-buildable. The impure read source
//! (the live LDAP/AD/Entra/Workspace client) is INJECTED through the vendor-free [`DirectorySource`]
//! seam; a hermetic [`FixtureDirectory`] implements the same seam for tests + the in-memory case.
//!
//! ## Safety floor (read-first, roadmap §3.2)
//! - Describe is **pure and credential-free**; reads touch nothing mutable.
//! - Every relation declares ONLY `SELECT` — directory *writes* (provisioning / deprovisioning
//!   identities, a much larger blast radius) are **out of scope** here and are not even expressible
//!   through this driver (a future ticket with its own preview/commit/irreversible analysis).
//! - The schema has **no credential column**, so a bind secret cannot surface through a directory
//!   path even by accident.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod schema;
mod source;

use qfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use qfs_plan::PlanApplier;

pub use schema::{
    directory_relation_schema, node_for_path, parse_group_ref, DirRelation, Provider,
    DIRECTORIES_MOUNT,
};
pub use source::{resolve_is_member, DirectoryError, DirectorySource, FixtureDirectory};

/// The identity-directory driver (roadmap §1.2). Pure introspection only — it owns NO state and NO
/// backend (the read source is injected from the binary leaf through [`DirectorySource`]).
/// Construct with [`DirectoryDriver::new`].
pub struct DirectoryDriver {
    // A directory read is a bounded LDAP/Graph query; it pushes nothing down (honest declaration,
    // RFD §6) — filtering/projection is the engine's work over the scanned rows.
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl Default for DirectoryDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectoryDriver {
    /// Construct the (pure) identity-directory driver.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pushdown: PushdownProfile::None,
            procs: Vec::new(),
        }
    }
}

/// The per-node capability set (RFD §5). Every directory relation is **read-only** in this slice —
/// `SELECT` and nothing else. Single source of truth shared by [`Driver::capabilities`] and the
/// parse-time verb gate, so a write verb is rejected structurally before a `Plan` exists.
#[must_use]
pub fn directory_relation_capabilities(_relation: DirRelation) -> Capabilities {
    Capabilities::from_verbs(&[Verb::Select])
}

impl Driver for DirectoryDriver {
    fn mount(&self) -> &str {
        DIRECTORIES_MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Pure: returns static schema data; never opens a directory connection or reads a credential.
        let (_provider, relation) =
            node_for_path(path.as_str()).ok_or_else(|| qfs_driver::CfsError::UnsupportedVerb {
                path: path.as_str().to_string(),
                verb: "DESCRIBE",
                supported: Vec::new(),
            })?;
        // groups/users/memberships are all relational tables (RFD §5 "four archetypes").
        Ok(NodeDesc::new(
            Archetype::RelationalTable,
            directory_relation_schema(relation),
        ))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        match node_for_path(path.as_str()) {
            Some((_provider, relation)) => directory_relation_capabilities(relation),
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
        // Read-first: like SysDriver/ServerDriver, the directory owns no impure write seam — there is
        // no directory mutation in this slice — so this is a no-op to satisfy the trait. The live
        // READ source is the injected DirectorySource, reached OUTSIDE the effect-plan applier path.
        &NoopApplier
    }
}

/// A no-op applier for the `Driver::applier()` contract slot (mirrors `SysDriver`'s). The directory
/// is read-only in this slice, so there is no real apply path; this exists only so `DirectoryDriver`
/// satisfies the introspective trait without pretending to own an impure write seam. It touches
/// nothing.
struct NoopApplier;

impl PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_plan::EffectNode,
    ) -> Result<qfs_plan::AppliedEffect, qfs_plan::ApplyError> {
        Ok(qfs_plan::AppliedEffect::new(node.id, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_driver::{check_capability, CfsError};
    use std::sync::Arc;

    /// G3 — the purity proof for the introspective half (the t58 analogue of
    /// `fixture_driver_introspection_is_pure` / `describe_sys_users_is_pure_no_db_no_creds`).
    /// `DESCRIBE /directories/google/groups` resolves to a stable typed schema with NO directory
    /// connection and NO creds: the driver owns no backend, and none of these methods take
    /// `&mut self`, return a future, or perform I/O — so a no-I/O round-trip IS the proof.
    #[test]
    fn describe_directories_is_pure_no_connection_no_creds() {
        let d = DirectoryDriver::new();
        assert_eq!(d.mount(), "/directories");

        let groups = d
            .describe(&Path::new("/directories/google/groups"))
            .unwrap();
        assert_eq!(groups.archetype, Archetype::RelationalTable);
        assert!(groups.schema.column("group").is_some());

        let users = d.describe(&Path::new("/directories/ldap/users")).unwrap();
        assert!(users.schema.column("user").is_some());
        // The schema carries NO secret/credential column — structurally cred-free.
        assert!(users.schema.column("password_hash").is_none());
        assert!(users.schema.column("bind_secret").is_none());

        let memberships = d
            .describe(&Path::new("/directories/ad/memberships"))
            .unwrap();
        assert!(memberships.schema.column("user").is_some());
        assert!(memberships.schema.column("group").is_some());

        // An unknown provider / relation is not describable (no panic).
        assert!(d.describe(&Path::new("/directories/nope/groups")).is_err());
        assert!(d.describe(&Path::new("/directories/google/nope")).is_err());
        assert!(d.describe(&Path::new("/directories")).is_err());
    }

    /// The redaction contract is structural: NO directory relation declares a column a secret could
    /// ride in (`password_hash`, `bind_secret`, `ciphertext`, …).
    #[test]
    fn directory_schemas_have_no_secret_column() {
        for relation in [
            DirRelation::Groups,
            DirRelation::Users,
            DirRelation::Memberships,
        ] {
            let schema = directory_relation_schema(relation);
            for forbidden in [
                "password_hash",
                "bind_secret",
                "secret",
                "ciphertext",
                "nonce",
                "token",
            ] {
                assert!(
                    schema.column(forbidden).is_none(),
                    "/directories {relation:?} must never expose `{forbidden}`"
                );
            }
        }
    }

    /// Capability golden gate: directory relations are READ-ONLY — `SELECT` passes, every write
    /// verb is rejected at the parse-time gate with a structured error (read-first, no provisioning).
    #[test]
    fn relations_are_read_only_writes_rejected_structurally() {
        let d = DirectoryDriver::new();
        let groups = Path::new("/directories/google/groups");
        assert!(check_capability(&d, &groups, Verb::Select).is_ok());
        for verb in [Verb::Insert, Verb::Upsert, Verb::Update, Verb::Remove] {
            let err = check_capability(&d, &groups, verb).unwrap_err();
            assert!(
                matches!(err, CfsError::UnsupportedVerb { .. }),
                "/directories must reject {} structurally (read-first)",
                verb.label()
            );
        }
    }

    /// The driver is object-safe (`Arc<dyn Driver>`) — the registries store trait objects (G2).
    #[test]
    fn directory_driver_is_object_safe() {
        let d: Arc<dyn Driver> = Arc::new(DirectoryDriver::new());
        assert_eq!(d.mount(), "/directories");
        // The default `id()` derives the plan DriverId from the mount.
        assert_eq!(d.id(), qfs_types::DriverId::new("directories"));
        let _seam: &dyn PlanApplier = d.applier();
    }
}
