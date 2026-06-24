//! Per-driver fakes (t38, RFD §6/§10): [`FakeBackend`] (a [`PlanApplier`] over an in-memory
//! [`FakeWorld`]) for the COMMIT leg + apply-twice idempotency, and [`NoCreds`] (injected
//! where a real `CredentialStore` would be — proves no token use).
//!
//! ## Built on the existing applier seam, not a parallel one
//! [`FakeBackend`] **is** a [`qfs_core::PlanApplier`] — the same trait the real runtime
//! interpreter ([`qfs_core::commit`]) drives and that `qfs-engine`'s `RecordingApplier`
//! already implements. So a fake backend exercises the *exact* COMMIT path a production
//! driver does (DESCRIBE → plan → COMMIT), with the World replaced by an in-memory map and
//! no creds. This is the consolidation of the ad-hoc `RecordingApplier`/`MockObjectBackend`/
//! fake-ApplyDriver patterns the driver tickets each grew.
//!
//! ## Idempotency (RFD §6): apply-twice converges
//! [`FakeWorld`] stores **rows-per-path**. `Upsert`/`Update`/`ServerConfigWrite` **replace**
//! the rows at a path (idempotent: applying the same plan twice converges to one copy);
//! `Insert` **appends** (not retry-safe — re-running grows the rows), which is exactly why
//! the closed core models `Upsert` distinctly. The `FakeWorld` is snapshotable so a test can
//! assert post-COMMIT state equality across a double-apply.

use std::collections::BTreeMap;

use qfs_core::{
    AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier, Row, RowBatch, Target,
};

/// In-memory, snapshotable World: rows-per-path. Owned data only; no I/O. A test seeds it,
/// drives a plan through [`FakeBackend`], then asserts `world()` post-COMMIT and idempotency.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FakeWorld {
    /// The rows currently at each VFS path (keyed by the canonical path string).
    rows: BTreeMap<String, Vec<Row>>,
}

impl FakeWorld {
    /// An empty World.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The rows currently at `path` (empty slice if the path was never written).
    #[must_use]
    pub fn rows_at(&self, path: &str) -> &[Row] {
        self.rows.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The number of distinct paths that hold rows.
    #[must_use]
    pub fn path_count(&self) -> usize {
        self.rows.len()
    }

    /// Replace the rows at `path` (the idempotent `Upsert`/`Update`/config-write semantics).
    fn set(&mut self, path: &str, rows: Vec<Row>) {
        self.rows.insert(path.to_string(), rows);
    }

    /// Append rows at `path` (the non-idempotent `Insert` semantics).
    fn append(&mut self, path: &str, mut rows: Vec<Row>) {
        self.rows
            .entry(path.to_string())
            .or_default()
            .append(&mut rows);
    }

    /// Remove all rows at `path` (the `Remove`/`Rm` semantics).
    fn clear(&mut self, path: &str) {
        self.rows.remove(path);
    }
}

/// A per-driver fake backend: a [`PlanApplier`] that applies effect nodes against an in-memory
/// [`FakeWorld`] instead of a live backend — capability-faithful for the COMMIT leg, with no
/// creds and no socket. `seed` pre-populates a path; `world()` exposes the post-COMMIT state.
#[derive(Debug, Clone, Default)]
pub struct FakeBackend {
    world: FakeWorld,
}

impl FakeBackend {
    /// A fresh fake backend over an empty World.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate `path` with `rows` (the DESCRIBE-time seed a read-then-write needs).
    pub fn seed(&mut self, path: &str, rows: Vec<Row>) {
        self.world.set(path, rows);
    }

    /// The post-COMMIT World — what a test asserts against (and snapshots across a double-
    /// apply to prove idempotency).
    #[must_use]
    pub fn world(&self) -> &FakeWorld {
        &self.world
    }

    /// The path a node's [`Target`] writes to, as a canonical string.
    fn node_path(target: &Target) -> String {
        target.path.as_str().to_string()
    }

    /// The rows a node carries (its [`RowBatch`] payload), cloned.
    fn node_rows(batch: &RowBatch) -> Vec<Row> {
        batch.rows.clone()
    }
}

impl PlanApplier for FakeBackend {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let path = Self::node_path(&node.target);
        let rows = Self::node_rows(&node.args);
        let affected = rows.len() as u64;
        match &node.kind {
            // Idempotent writes REPLACE the rows at the path — apply-twice converges.
            EffectKind::Upsert | EffectKind::Update | EffectKind::ServerConfigWrite { .. } => {
                self.world.set(&path, rows);
            }
            // INSERT APPENDS — deliberately NOT retry-safe (re-running grows the rows), which
            // is why the closed core models Upsert distinctly (RFD §6 recovery).
            EffectKind::Insert => {
                self.world.append(&path, rows);
            }
            // REMOVE clears the path's rows.
            EffectKind::Remove => {
                self.world.clear(&path);
            }
            // Read/List are pure dependency nodes — no World mutation, just an accounted apply.
            EffectKind::Read | EffectKind::List => {}
            // CALL is an irreducible procedure: the fake records nothing in the World (a real
            // backend's side effect is opaque to the rows model), it just accounts the apply.
            EffectKind::Call(_) => {}
            // `EffectKind` is `#[non_exhaustive]`: a future kind defaults to a no-op apply
            // (accounted, World untouched) rather than failing to compile — additive-safe.
            _ => {}
        }
        Ok(AppliedEffect::new(node.id, affected))
    }
}

// ---------------------------------------------------------------------------
// NoCreds — the injected no-token credential source (RFD §10).
// ---------------------------------------------------------------------------

/// The credential source injected where a real `CredentialStore` would be — it holds **no
/// token** and serves **none**. A test that "passes" while wired to [`NoCreds`] provably used
/// no credential (RFD §10 least-privilege): the only honest answer it can give is "absent".
///
/// This is the test-side counterpart of the no-network guard: the no-network guard proves no
/// socket was opened; `NoCreds` proves no token was read. Together a green test certifies the
/// operation under test is pure (PREVIEW / plan-assertion) — no secret and no I/O.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCreds;

impl NoCreds {
    /// Construct the no-credential source.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Always returns `None` — there is no credential to serve. A caller that *requires* one
    /// must fail with a structured "no credential" error rather than run unauthenticated, so a
    /// PREVIEW/plan-assertion path (which needs no token) stays green while a COMMIT path that
    /// reaches for a token fails loudly.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_core::{Affected, DriverId, NodeId, Plan, PlanBuilder, Value, VfsPath};

    fn row(text: &str) -> Row {
        Row::new(vec![Value::Text(text.to_string())])
    }

    fn batch(rows: Vec<Row>) -> RowBatch {
        use qfs_core::{Column, ColumnType, Schema};
        let schema = Schema::new(vec![Column::new("v", ColumnType::Text, false)]);
        RowBatch::new(schema, rows)
    }

    fn upsert_node(id: u32, path: &str, rows: Vec<Row>) -> EffectNode {
        EffectNode::new(
            NodeId(id),
            EffectKind::Upsert,
            Target::new(DriverId::new("fake"), VfsPath::new(path)),
        )
        .with_args(batch(rows))
        .with_affected(Affected::Exact(1))
    }

    fn one_upsert_plan(path: &str, rows: Vec<Row>) -> Plan {
        let mut b = PlanBuilder::new();
        b.push(upsert_node(0, path, rows));
        b.build()
    }

    #[test]
    fn fake_world_seed_and_post_commit_state() {
        let mut be = FakeBackend::new();
        be.seed("/fake/t", vec![row("seed")]);
        assert_eq!(be.world().rows_at("/fake/t").len(), 1);

        let plan = one_upsert_plan("/fake/t", vec![row("a"), row("b")]);
        let mut applier = be;
        let report = qfs_core::commit(&plan, &mut applier, |_| {});
        assert!(report.is_complete());
        // Upsert REPLACED the seeded row with the two new rows.
        assert_eq!(applier.world().rows_at("/fake/t"), &[row("a"), row("b")]);
    }

    #[test]
    fn upsert_apply_twice_is_idempotent() {
        let plan = one_upsert_plan("/fake/t", vec![row("x")]);
        let mut be = FakeBackend::new();

        let _ = qfs_core::commit(&plan, &mut be, |_| {});
        let after_first = be.world().clone();
        let _ = qfs_core::commit(&plan, &mut be, |_| {});
        let after_second = be.world().clone();

        // Apply-twice CONVERGES: the World is byte-identical after the second apply (RFD §6).
        assert_eq!(after_first, after_second);
        assert_eq!(be.world().rows_at("/fake/t"), &[row("x")]);
    }

    #[test]
    fn insert_apply_twice_is_not_idempotent_by_design() {
        // INSERT appends — re-running grows the rows. This is the contrast that justifies the
        // distinct Upsert verb; the fake faithfully models it.
        let mut b = PlanBuilder::new();
        b.push(
            EffectNode::new(
                NodeId(0),
                EffectKind::Insert,
                Target::new(DriverId::new("fake"), VfsPath::new("/fake/log")),
            )
            .with_args(batch(vec![row("e")]))
            .with_affected(Affected::Exact(1)),
        );
        let plan = b.build();

        let mut be = FakeBackend::new();
        let _ = qfs_core::commit(&plan, &mut be, |_| {});
        let _ = qfs_core::commit(&plan, &mut be, |_| {});
        assert_eq!(be.world().rows_at("/fake/log").len(), 2, "insert grows");
    }

    #[test]
    fn no_creds_serves_no_token() {
        let nc = NoCreds::new();
        assert!(nc.token().is_none());
        // The redacting Debug never prints a token shape (there is none to print).
        assert!(!format!("{nc:?}").contains("Bearer"));
    }
}
