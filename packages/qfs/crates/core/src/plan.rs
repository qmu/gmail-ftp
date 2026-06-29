//! The **pushdown integration seam** (ticket t14): wire a query AST through the
//! `qfs-pushdown` planner using the live [`MountRegistry`], so independent native
//! [`ScanNode`](qfs_pushdown::ScanNode)s surface for T10's batcher.
//!
//! This is the boundary the t14 ticket asks for: `partition_by_source → engine` wired
//! into the engine. `qfs-core` owns the registry (the source of driver pushdown profiles
//! and `describe` schemas), so it is the right home for the adapter that turns the
//! registry into a `qfs_pushdown::SourceRegistry` and lowers a parsed [`Pipeline`] into a
//! [`PhysicalPlan`]. The pushdown planner itself stays pure (no registry, no I/O); this
//! adapter feeds it owned data.
//!
//! Predicates are sourced from the AST (O-t07-3): the lowering runs over the parser
//! [`Pipeline`], not the schema-threading `PlanSource`.

use qfs_driver::{Driver, Path, PushdownProfile};
use qfs_parser::{Pipeline, Statement};
use qfs_pushdown::{
    lower_query, partition_by_source, LowerError, PhysicalPlan, PlanError, SourceId, SourceRegistry,
};
use qfs_types::Schema;

use crate::registry::MountRegistry;

/// A structured error from the pushdown integration: either the AST could not be lowered
/// (`LowerError`) or the partition rejected at plan time (`PlanError`). A query that does
/// not route to any mounted driver is its own arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PushdownError {
    /// The query AST could not be lowered into the planner IR (a non-predicate `WHERE`,
    /// an unsupported `JOIN ON`, etc.).
    Lower(LowerError),
    /// The source-split rejected at plan time (unknown source, capability denial).
    Plan(PlanError),
    /// The statement was not a pure read query, so there is nothing to push down.
    NotAQuery,
}

impl PushdownError {
    /// A stable, machine-readable code (RFD §5).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            PushdownError::Lower(e) => e.code(),
            PushdownError::Plan(e) => e.code(),
            PushdownError::NotAQuery => "not_a_query",
        }
    }
}

impl From<LowerError> for PushdownError {
    fn from(e: LowerError) -> Self {
        PushdownError::Lower(e)
    }
}

impl From<PlanError> for PushdownError {
    fn from(e: PlanError) -> Self {
        PushdownError::Plan(e)
    }
}

/// Build a [`qfs_pushdown::SourceRegistry`] from the live [`MountRegistry`]: each mounted
/// driver contributes its [`Driver::id`] as a [`SourceId`] and its [`Driver::pushdown`]
/// profile. The synthetic `(values)` source (inline `VALUES`) is registered as a
/// `None`-pushdown local source so a `FROM VALUES` partitions without an "unknown source"
/// error.
#[must_use]
pub fn source_registry(mounts: &MountRegistry) -> SourceRegistry {
    let mut reg = SourceRegistry::new();
    for driver in mounts.drivers() {
        reg.register(SourceId::new(driver.id().as_str()), *driver.pushdown());
    }
    reg.register(SourceId::new("(values)"), PushdownProfile::None);
    reg
}

/// Lower + partition a query [`Statement`] into a [`PhysicalPlan`] using the live mount
/// registry (ticket t14 entry point). The source of each `/driver/...` leaf is the
/// driver its first path segment routes to; its schema comes from the driver's pure
/// `describe` (no I/O). A non-query statement is [`PushdownError::NotAQuery`].
///
/// # Errors
/// [`PushdownError`] if the AST cannot be lowered or the split is rejected at plan time.
pub fn plan_query(stmt: &Statement, mounts: &MountRegistry) -> Result<PhysicalPlan, PushdownError> {
    let Statement::Query(pipeline) = stmt else {
        return Err(PushdownError::NotAQuery);
    };
    plan_pipeline(pipeline, mounts)
}

/// Lower + partition a [`Pipeline`] directly (the inner form `plan_query` delegates to).
///
/// # Errors
/// [`PushdownError`] if the AST cannot be lowered or the split is rejected at plan time.
pub fn plan_pipeline(
    pipeline: &Pipeline,
    mounts: &MountRegistry,
) -> Result<PhysicalPlan, PushdownError> {
    let source_of = |segs: &[String]| -> SourceId {
        let full = render_path(segs);
        match mounts.resolve_path(&full) {
            Some((driver, _)) => SourceId::new(driver.id().as_str()),
            // An unrouted FROM resolves to a synthetic source id (its first segment), so
            // the partitioner surfaces a structured `unknown_source` rather than panicking.
            None => SourceId::new(segs.first().cloned().unwrap_or_default()),
        }
    };
    let schema_of = |src: &SourceId| -> Schema {
        // Re-resolve the driver by id to fetch its described schema. A `(values)` or
        // unrouted source has no driver; it gets an empty (late-bound) schema.
        for driver in mounts.drivers() {
            if driver.id().as_str() == src.as_str() {
                return describe_root(driver.as_ref());
            }
        }
        Schema::empty()
    };

    let logical = lower_query(pipeline, &source_of, &schema_of)?;
    let reg = source_registry(mounts);
    let physical = partition_by_source(&logical, &reg)?;
    Ok(physical)
}

/// Describe a driver's root node schema via its **pure** `describe` (no I/O). An
/// undescribable root degrades to an empty (late-bound) schema, keeping lowering total.
fn describe_root(driver: &dyn Driver) -> Schema {
    let root = format!("/{}", driver.id().as_str());
    driver
        .describe(&Path::new(root))
        .map(|d| d.schema)
        .unwrap_or_else(|_| Schema::empty())
}

/// Render a logical segment list into a `/seg/seg` router path.
fn render_path(segments: &[String]) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let mut s = String::new();
    for seg in segments {
        s.push('/');
        s.push_str(seg);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_driver::{
        AliasFn, Archetype, Capabilities, CfsError, NodeDesc, ProcSig, VersionSupport,
    };
    use qfs_parser::parse_statement;
    use qfs_plan::PlanApplier;
    use qfs_pushdown::explain;
    use qfs_types::{Column, ColumnType};
    use std::sync::Arc;

    struct FakeDriver {
        mount: String,
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

    impl FakeDriver {
        fn new(mount: &str, pushdown: PushdownProfile) -> Self {
            Self {
                mount: mount.to_string(),
                pushdown,
                applier: NoopApplier,
            }
        }
    }

    impl Driver for FakeDriver {
        fn mount(&self) -> &str {
            &self.mount
        }
        fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
            Ok(NodeDesc::new(
                Archetype::RelationalTable,
                Schema::new(vec![
                    Column::new("id", ColumnType::Int, false),
                    Column::new("name", ColumnType::Text, true),
                ]),
            ))
        }
        fn capabilities(&self, _path: &Path) -> Capabilities {
            Capabilities::none().select()
        }
        fn procedures(&self) -> &[ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &self.pushdown
        }
        fn prelude(&self) -> &[AliasFn] {
            &[]
        }
        fn version_support(&self, _path: &Path) -> VersionSupport {
            VersionSupport::None
        }
        fn applier(&self) -> &dyn PlanApplier {
            &self.applier
        }
    }

    fn registry() -> MountRegistry {
        let mut reg = MountRegistry::new();
        reg.register(Arc::new(FakeDriver::new("/db", PushdownProfile::Full)))
            .unwrap();
        reg.register(Arc::new(FakeDriver::new("/git", PushdownProfile::None)))
            .unwrap();
        reg
    }

    #[test]
    fn single_source_query_lowers_to_one_scan_via_registry() {
        let mounts = registry();
        let stmt = parse_statement("/db/users |> WHERE id > 0 |> SELECT id").unwrap();
        let phys = plan_query(&stmt, &mounts).unwrap();
        assert_eq!(phys.scan_count(), 1);
        // /db is Full → everything pushes.
        assert_eq!(explain(&phys), "Scan[db] pushed=[where, project(id)]\n");
    }

    #[test]
    fn cross_source_join_via_registry_federates() {
        let mounts = registry();
        let stmt = parse_statement("/db/users |> JOIN /git/commits ON id == id").unwrap();
        let phys = plan_query(&stmt, &mounts).unwrap();
        assert_eq!(phys.scan_count(), 2);
        // /db pushes nothing extra (bare scan), /git is None (bare). The join federates.
        assert!(explain(&phys).starts_with("Combine[HashJoin(id = id)]"));
    }

    #[test]
    fn none_source_leaves_residual_local_via_registry() {
        let mounts = registry();
        let stmt = parse_statement("/git/log |> WHERE id > 0").unwrap();
        let phys = plan_query(&stmt, &mounts).unwrap();
        // /git is None → the WHERE is a local residual.
        assert_eq!(explain(&phys), "Combine[Filter]\n  Scan[git] pushed=[]\n");
    }

    #[test]
    fn non_query_statement_is_rejected() {
        let mounts = registry();
        let stmt = parse_statement("INSERT INTO /db/users VALUES (1, 'a')").unwrap();
        let err = plan_query(&stmt, &mounts).unwrap_err();
        assert_eq!(err.code(), "not_a_query");
    }
}
