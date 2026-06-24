//! `qfs-core::describe` — the **`DESCRIBE <path>` output contract** (ticket t39, RFD §5).
//!
//! [`DescribeReport`] is the single owned DTO an AI agent reads as the first step of the
//! uniform loop **DESCRIBE → write a qfs statement → PREVIEW → COMMIT**. It *formalizes* what
//! the t13 [`qfs_driver::Driver`] introspective half already exposes ([`Driver::describe`] /
//! [`Driver::capabilities`] / [`Driver::procedures`] / [`Driver::prelude`] /
//! [`Driver::pushdown`]) into one JSON shape, so the agent learns one contract instead of N
//! SDKs.
//!
//! ## Owned DTOs only — no vendor SDK type leaks (RFD §9)
//! Every field reuses an existing **owned** workspace type — [`qfs_types::Column`] for columns,
//! [`Capabilities`] for the supported universal verbs, [`ProcSig`] for `CALL` signatures,
//! [`AliasFn`] for prelude pure fns — plus the thin local [`PushdownSummary`] derived from a
//! driver's [`PushdownProfile`]. No vendor handle, no token, no credential ever reaches this
//! report (RFD §10): it carries schema + capabilities only.
//!
//! ## `Serialize` only (the JSON is the agent-facing contract)
//! The report derives `serde::Serialize` so `qfs describe <path> -json` emits a stable shape an
//! agent parses; it intentionally does **not** derive `Deserialize` (qfs never reads a report
//! back — it is produced from a live driver, never reconstructed from untrusted JSON).
//!
//! ## Building one (the introspective fold)
//! [`DescribeReport::from_driver`] folds a node's archetype, schema, capabilities, procedures,
//! prelude, and pushdown into the report by calling **only the pure introspective half** of the
//! contract — it never reaches [`Driver::applier`], so building a report touches no World (no
//! creds, no I/O, no network). It also attaches a per-[`Archetype`] **native-verb hint** so the
//! agent sees the FS/SQL-shaped verbs each archetype answers to.

use qfs_driver::{
    AliasFn, Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile,
};
use qfs_types::Column;
use serde::Serialize;

/// The `DESCRIBE <path>` output contract (RFD §5): everything an AI agent needs to write the
/// next qfs statement against a node, in one owned, `Serialize`-only DTO.
///
/// Built with [`DescribeReport::from_driver`], which calls only the pure introspective half of
/// the [`Driver`] contract — so a report is produced with **no creds, no I/O, no network**.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DescribeReport {
    /// The described node path, e.g. `/mail/drafts`.
    pub path: String,
    /// How this node maps onto qfs's uniform model (Blob / Relational / Append / ObjectGraph).
    pub archetype: Archetype,
    /// The FS/SQL-shaped native verbs this archetype answers to — the agent's one-line hint for
    /// "what does writing against this node look like". Derived from [`archetype_hint`].
    pub native_verbs: &'static str,
    /// The node's typed columns (name + [`qfs_types::ColumnType`] + nullability), reused from the
    /// canonical [`qfs_types::Schema`] — the agent reads `columns[i].name` / `.ty` directly.
    pub columns: Vec<Column>,
    /// Which universal verbs this node supports (the parse-time capability gate, RFD §5).
    pub verbs: Capabilities,
    /// The `CALL driver.action(..)` signatures this driver declares (RFD §3).
    pub procedures: Vec<ProcSig>,
    /// The prelude pure-fn aliases in scope for this driver (e.g. `SEND -> mail.send`).
    pub aliases: Vec<AliasFn>,
    /// What the source can push down natively (the planner's pushdown input, RFD §6).
    pub pushdown: PushdownSummary,
}

impl DescribeReport {
    /// Fold a driver's pure introspective half into a [`DescribeReport`] for `path`.
    ///
    /// Calls only `describe` / `capabilities` / `procedures` / `prelude` / `pushdown` — the
    /// introspective methods that return owned data — and **never** [`Driver::applier`]. So
    /// building a report performs no I/O, resolves no credentials, and opens no socket (RFD §3
    /// purity invariant from the DTO side).
    ///
    /// # Errors
    /// Propagates the driver's [`qfs_driver::CfsError`] if `path` does not resolve to a
    /// describable node (e.g. a mount root with no relation) — the agent-legible failure path.
    pub fn from_driver(driver: &dyn Driver, path: &Path) -> Result<Self, qfs_driver::CfsError> {
        let NodeDesc {
            archetype, schema, ..
        } = driver.describe(path)?;
        Ok(Self {
            path: path.as_str().to_string(),
            archetype,
            native_verbs: archetype_hint(archetype),
            columns: schema.columns,
            verbs: driver.capabilities(path),
            procedures: driver.procedures().to_vec(),
            aliases: driver.prelude().to_vec(),
            pushdown: PushdownSummary::from_profile(driver.pushdown()),
        })
    }
}

/// The per-[`Archetype`] **native-verb hint** the agent reads (RFD §5, "Four archetypes"): the
/// FS/SQL-shaped vocabulary each archetype is modeled on. A stable, owned `&'static str` — never
/// prose the loop needs to special-case (the four steps stay identical across drivers).
#[must_use]
pub const fn archetype_hint(archetype: Archetype) -> &'static str {
    match archetype {
        Archetype::BlobNamespace => "ls cp mv rm (+ universal upsert/remove)",
        Archetype::RelationalTable => "SELECT JOIN INSERT UPDATE UPSERT",
        Archetype::AppendLog => "SELECT(tail) INSERT(append)",
        Archetype::ObjectGraphWorkflow => "SELECT INSERT UPDATE REMOVE + CALL driver.action",
        // `Archetype` is `#[non_exhaustive]` (cross-crate), so a wildcard is mandatory. A
        // future archetype gets an honest, generic universal-verb hint until the contract
        // declares its native vocabulary here.
        _ => "SELECT INSERT UPDATE REMOVE (universal verbs)",
    }
}

/// The flattened pushdown summary the agent reads — the boolean intent of a driver's
/// [`PushdownProfile`] (RFD §6), so the report is a flat shape (`where_`/`project`/…) rather than
/// an externally-tagged `Partial { … }` union the agent must branch on. Owned data only.
///
/// `None` flattens to all-`false`, `Full` to all-`true`, and `Partial { … }` carries each
/// declared flag through unchanged — queried via the profile's own `supports_*` accessors so the
/// summary cannot drift from the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct PushdownSummary {
    /// Can push down `WHERE` predicates.
    pub where_: bool,
    /// Can push down projection (a `SELECT` column subset).
    pub project: bool,
    /// Can push down `LIMIT`.
    pub limit: bool,
    /// Can push down `ORDER BY`.
    pub order: bool,
    /// Can push down a join.
    pub join: bool,
    /// Can push down aggregation (`COUNT`/`SUM`/…).
    pub aggregate: bool,
    /// Can push down `DISTINCT` deduplication.
    pub distinct: bool,
    /// Can push down `GROUP BY` bucketing.
    pub group_by: bool,
}

impl PushdownSummary {
    /// Flatten a [`PushdownProfile`] into the boolean summary, querying it through the profile's
    /// own `supports_*` intent accessors (so a new pushdown flag cannot drift out of sync).
    #[must_use]
    pub const fn from_profile(profile: &PushdownProfile) -> Self {
        Self {
            where_: profile.supports_where(),
            project: profile.supports_project(),
            limit: profile.supports_limit(),
            order: profile.supports_order(),
            join: profile.supports_join(),
            aggregate: profile.supports_aggregate(),
            distinct: profile.supports_distinct(),
            group_by: profile.supports_group_by(),
        }
    }

    /// Whether the source pushes nothing down (every flag `false`) — the agent's "this scan runs
    /// locally; filter/project happen in qfs" signal.
    #[must_use]
    pub const fn is_local_only(&self) -> bool {
        !(self.where_
            || self.project
            || self.limit
            || self.order
            || self.join
            || self.aggregate
            || self.distinct
            || self.group_by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_driver::{NodeDesc, Verb, VersionSupport};
    use qfs_plan::PlanApplier;
    use qfs_types::{Column, ColumnType, Schema};

    /// A no-I/O fixture driver (no creds, no socket): a relational node with a declared,
    /// irreversible `send` proc, a `SEND` prelude alias, and a partial pushdown — exactly the
    /// shape `from_driver` folds into a report.
    struct FixtureDriver {
        procs: Vec<ProcSig>,
        prelude: Vec<AliasFn>,
        pushdown: PushdownProfile,
        applier: NoopApplier,
    }

    #[derive(Default)]
    struct NoopApplier;

    impl PlanApplier for NoopApplier {
        fn apply(
            &mut self,
            node: &qfs_plan::EffectNode,
        ) -> Result<qfs_plan::AppliedEffect, qfs_plan::ApplyError> {
            Ok(qfs_plan::AppliedEffect::new(node.id, 0))
        }
    }

    impl FixtureDriver {
        fn new() -> Self {
            Self {
                procs: vec![ProcSig::new("send")
                    .with_params(vec![qfs_driver::Param::new("to", ColumnType::Text)])
                    .irreversible(true)],
                prelude: vec![AliasFn::new("SEND", "fix.send")],
                pushdown: PushdownProfile::Partial {
                    where_: true,
                    project: false,
                    limit: true,
                    order: false,
                    join: false,
                    aggregate: false,
                    distinct: false,
                    group_by: false,
                },
                applier: NoopApplier,
            }
        }
    }

    impl Driver for FixtureDriver {
        fn mount(&self) -> &str {
            "/fix"
        }
        fn describe(&self, _path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
            Ok(NodeDesc::new(
                Archetype::RelationalTable,
                Schema::new(vec![
                    Column::new("id", ColumnType::Int, false),
                    Column::new("name", ColumnType::Text, true),
                ]),
            ))
        }
        fn capabilities(&self, _path: &Path) -> Capabilities {
            Capabilities::from_verbs(&[Verb::Select, Verb::Insert, Verb::Upsert])
        }
        fn procedures(&self) -> &[ProcSig] {
            &self.procs
        }
        fn pushdown(&self) -> &PushdownProfile {
            &self.pushdown
        }
        fn prelude(&self) -> &[AliasFn] {
            &self.prelude
        }
        fn version_support(&self, _path: &Path) -> VersionSupport {
            VersionSupport::Versioned
        }
        fn applier(&self) -> &dyn PlanApplier {
            &self.applier
        }
    }

    #[test]
    fn from_driver_folds_the_introspective_half() {
        let d = FixtureDriver::new();
        let report = DescribeReport::from_driver(&d, &Path::new("/fix/rel")).unwrap();

        assert_eq!(report.path, "/fix/rel");
        assert_eq!(report.archetype, Archetype::RelationalTable);
        assert_eq!(report.native_verbs, "SELECT JOIN INSERT UPDATE UPSERT");
        assert_eq!(report.columns.len(), 2);
        assert_eq!(report.columns[0].name, "id");
        assert!(report.verbs.select && report.verbs.insert && report.verbs.upsert);
        assert!(!report.verbs.remove);
        assert_eq!(report.procedures.len(), 1);
        assert!(report.procedures[0].irreversible);
        assert_eq!(report.aliases.len(), 1);
        assert_eq!(report.aliases[0].name, "SEND");
        assert!(report.pushdown.where_ && report.pushdown.limit);
        assert!(!report.pushdown.project);
    }

    #[test]
    fn pushdown_summary_flattens_endpoints() {
        assert!(PushdownSummary::from_profile(&PushdownProfile::None).is_local_only());
        let full = PushdownSummary::from_profile(&PushdownProfile::Full);
        assert!(full.where_ && full.aggregate && full.group_by);
        assert!(!full.is_local_only());
    }

    /// The report's JSON projection is stable for AI consumption (`-json`): owned DTOs serialize
    /// in declaration order. Pins the agent-facing wire shape.
    #[test]
    fn report_json_shape_is_stable() {
        let d = FixtureDriver::new();
        let report = DescribeReport::from_driver(&d, &Path::new("/fix/rel")).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        // Spot-check the load-bearing keys an agent reads (the full table is pinned by the
        // qfs-skill golden corpus; here we assert the contract surface exists and is flat).
        assert!(json.contains("\"path\":\"/fix/rel\""));
        assert!(json.contains("\"archetype\":\"relational_table\""));
        assert!(json.contains("\"native_verbs\":\"SELECT JOIN INSERT UPDATE UPSERT\""));
        assert!(json.contains("\"pushdown\":{\"where_\":true"));
        // No vendor type / credential shape leaked: the report is schema + capabilities only.
        assert!(!json.contains("token"));
        assert!(!json.contains("Bearer"));
    }
}
