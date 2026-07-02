//! **MountAdapter** — re-mount a driver under a caller-chosen path segment (ADR 0008 §4).
//!
//! A cloud mount is connect-created: `qfs connect /mail2 gmail home@x.com` must register a
//! gmail-kind driver whose `driver.id()` is `mail2`, because all three runtime registries key
//! by `driver.id()` — the `MountRegistry` (describe/plan), the `ReadRegistry` (scan, keyed by
//! the pushdown `SourceId`), and the `DriverRegistry` (apply). Path reconstruction
//! (`/{driver.id()}/…`), pushdown source ids, `CALL <id>.proc` qualification, and interpreter
//! grouping all derive from it. The driver crates themselves stay single-mount (`/mail`); this
//! module supplies the three thin wrappers that re-home one at registration time:
//!
//! - [`MountDriver`] wraps a `qfs_core::Driver`: it reports the custom `mount()` (so the
//!   default `id()` derivation yields the segment), rewrites the mount prefix on every inbound
//!   path, rewrites plan targets / `CALL` qualifiers back on the way out, and precomputes an
//!   owned re-qualified `prelude()` (`SEND → mail2.send`).
//! - [`MountReadDriver`] wraps a `qfs_exec::ReadDriver`: it rewrites `ScanNode.source` and the
//!   scan path to the inner driver's own mount before delegating.
//! - [`MountApplyDriver`] wraps a `qfs_runtime::ApplyDriver`: it rewrites every
//!   `EffectInput.target` (driver id + path prefix) and `Call` qualifier inbound.
//!
//! The shared prefix arithmetic lives in [`MountRemap`]; a registration site builds one remap
//! per mount and hands clones to the three wrappers. Everything here is owned data and pure
//! rewriting — the wrapped driver keeps sole custody of credentials and I/O.

use std::sync::Arc;

use qfs_core::{
    AliasFn, Capabilities, CfsError, Driver, DriverId, EffectKind, NodeDesc, Path, Plan,
    PlanApplier, ProcId, ProcSig, PushdownProfile, RowBatch, Verb, VersionSupport,
};
use qfs_exec::ReadDriver;
use qfs_pushdown::{ScanNode, SourceId};
use qfs_runtime::{ApplyCx, ApplyDriver, EffectError, EffectInput, EffectOutput};

/// The prefix arithmetic between an **outer** (registered) mount and the **inner** driver's
/// id-keyed namespace, plus the derived driver-id pair for `CALL` qualifier rewriting. Cheap to
/// clone; one instance is shared by the three facet wrappers of a mount.
///
/// The inner side is keyed by the wrapped driver's **id**, not its `mount()`: every path the
/// engine hands a driver is reconstructed as `/{driver.id()}/…` (plan/resolve/eval), so the
/// namespace a driver's describe/read/apply surfaces actually speak is `/<id>` — which differs
/// from the mount for the one renamed driver (ga: mount `/google-analytics`, id `ga`).
#[derive(Debug, Clone)]
pub struct MountRemap {
    outer_mount: String,
    inner_prefix: String,
    outer_id: String,
    inner_id: String,
}

/// Replace the leading mount `from` with `to` iff `path` sits inside that mount — the
/// boundary after the prefix must be a segment (`/`), a version coordinate (`@`), or the end
/// of the path, so `/mail` never captures `/mail2/…`.
fn remap_prefix(path: &str, from: &str, to: &str) -> Option<String> {
    let rest = path.strip_prefix(from)?;
    if rest.is_empty() || rest.starts_with('/') || rest.starts_with('@') {
        Some(format!("{to}{rest}"))
    } else {
        None
    }
}

impl MountRemap {
    /// Build the remap between `outer_mount` (the connect-created mount, e.g. `/mail2`) and
    /// `inner_id` (the wrapped driver's plan identity, e.g. `mail`). The outer mount must be a
    /// non-empty absolute path; the inner id must be a non-empty bare id (no leading `/`).
    ///
    /// # Errors
    /// [`CfsError::InvalidPath`] if the outer mount is empty or not absolute, or the inner id is
    /// empty / not a bare id.
    pub fn new(outer_mount: &str, inner_id: &str) -> Result<Self, CfsError> {
        let outer = Path::parse(outer_mount)?;
        if inner_id.is_empty() || inner_id.starts_with('/') {
            return Err(CfsError::InvalidPath {
                path: inner_id.to_string(),
                reason: "inner driver id must be a non-empty bare id",
            });
        }
        Ok(Self {
            outer_id: outer
                .as_str()
                .strip_prefix('/')
                .unwrap_or(outer.as_str())
                .to_string(),
            inner_prefix: format!("/{inner_id}"),
            inner_id: inner_id.to_string(),
            outer_mount: outer.as_str().to_string(),
        })
    }

    /// The outer (registered) mount, e.g. `/mail2`.
    #[must_use]
    pub fn outer_mount(&self) -> &str {
        &self.outer_mount
    }

    /// The outer driver id the wrappers register under, e.g. `mail2`.
    #[must_use]
    pub fn outer_id(&self) -> DriverId {
        DriverId::new(&self.outer_id)
    }

    /// Rewrite an outer-mount path onto the inner driver's mount (`/mail2/x` → `/mail/x`).
    /// A path outside the outer mount (or a non-path like a draft handle) passes unchanged.
    #[must_use]
    pub fn path_in(&self, path: &str) -> String {
        remap_prefix(path, &self.outer_mount, &self.inner_prefix)
            .unwrap_or_else(|| path.to_string())
    }

    /// Rewrite an inner-mount path back onto the outer mount (`/mail/x` → `/mail2/x`).
    #[must_use]
    pub fn path_out(&self, path: &str) -> String {
        remap_prefix(path, &self.inner_prefix, &self.outer_mount)
            .unwrap_or_else(|| path.to_string())
    }

    /// Re-qualify an inner `CALL` name outward (`mail.send` → `mail2.send`).
    #[must_use]
    pub fn proc_out(&self, qualified: &str) -> String {
        match qualified.strip_prefix(&self.inner_id) {
            Some(rest) if rest.starts_with('.') => format!("{}{rest}", self.outer_id),
            _ => qualified.to_string(),
        }
    }

    /// Re-qualify an outer `CALL` name inward (`mail2.send` → `mail.send`).
    #[must_use]
    pub fn proc_in(&self, qualified: &str) -> String {
        match qualified.strip_prefix(&self.outer_id) {
            Some(rest) if rest.starts_with('.') => format!("{}{rest}", self.inner_id),
            _ => qualified.to_string(),
        }
    }

    fn kind_in(&self, kind: &EffectKind) -> EffectKind {
        match kind {
            EffectKind::Call(proc) => EffectKind::Call(ProcId::new(self.proc_in(&proc.0))),
            other => other.clone(),
        }
    }

    /// Rewrite one effect input inward: the target driver id, the target path prefix, and a
    /// `Call` qualifier all move onto the inner driver's namespace.
    #[must_use]
    fn effect_in(&self, effect: &EffectInput) -> EffectInput {
        let mut e = effect.clone();
        if e.target.driver.as_str() == self.outer_id {
            e.target.driver = DriverId::new(&self.inner_id);
        }
        e.target.path = qfs_core::VfsPath::new(self.path_in(e.target.path.as_str()));
        e.kind = self.kind_in(&e.kind);
        e
    }

    /// Rewrite a scan inward: the pushdown `SourceId` and the addressed path move onto the
    /// inner driver's namespace.
    #[must_use]
    fn scan_in(&self, scan: &ScanNode) -> ScanNode {
        let mut s = scan.clone();
        if s.source.as_str() == self.outer_id {
            s.source = SourceId::new(&self.inner_id);
        }
        s.path = self.path_in(&s.path);
        s
    }

    /// Rewrite a driver-lowered plan outward: every node's target driver id, target path
    /// prefix, and `Call` qualifier move onto the outer namespace, so the runtime routes the
    /// effects back through the wrappers registered under the outer id.
    #[must_use]
    fn plan_out(&self, mut plan: Plan) -> Plan {
        for node in &mut plan.nodes {
            if node.target.driver.as_str() == self.inner_id {
                node.target.driver = DriverId::new(&self.outer_id);
            }
            node.target.path = qfs_core::VfsPath::new(self.path_out(node.target.path.as_str()));
            if let EffectKind::Call(proc) = &node.kind {
                node.kind = EffectKind::Call(ProcId::new(self.proc_out(&proc.0)));
            }
        }
        plan
    }
}

/// A [`Driver`] re-mounted under a custom segment. `mount()` reports the outer mount, so the
/// trait's default `id()` derivation yields the outer segment; every path-taking method
/// rewrites inbound, and `plan_write` rewrites its produced plan back outbound.
pub struct MountDriver {
    remap: MountRemap,
    inner: Arc<dyn Driver>,
    /// Owned, re-qualified prelude (`SEND → mail2.send`) — precomputed because the trait
    /// returns a borrowed slice.
    prelude: Vec<AliasFn>,
}

impl MountDriver {
    /// Wrap `inner` so it answers for `outer_mount`. The remap is derived from the inner
    /// driver's own `id()` (the id-keyed namespace its paths are reconstructed under).
    ///
    /// # Errors
    /// [`CfsError::InvalidPath`] if `outer_mount` is empty or not absolute.
    pub fn new(outer_mount: &str, inner: Arc<dyn Driver>) -> Result<Self, CfsError> {
        let remap = MountRemap::new(outer_mount, inner.id().as_str())?;
        let prelude = inner
            .prelude()
            .iter()
            .map(|a| AliasFn::new(a.name.clone(), remap.proc_out(&a.desugars_to)))
            .collect();
        Ok(Self {
            remap,
            inner,
            prelude,
        })
    }

    /// The remap this wrapper was built with — a registration site clones it into the read
    /// and apply facet wrappers so all three facets share one prefix arithmetic.
    #[must_use]
    pub fn remap(&self) -> &MountRemap {
        &self.remap
    }
}

impl Driver for MountDriver {
    fn mount(&self) -> &str {
        &self.remap.outer_mount
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, CfsError> {
        self.inner
            .describe(&Path::new(self.remap.path_in(path.as_str())))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        self.inner
            .capabilities(&Path::new(self.remap.path_in(path.as_str())))
    }

    fn procedures(&self) -> &[ProcSig] {
        // Procedure names are unqualified (`send`); the resolver qualifies them with this
        // wrapper's id, so they pass through untouched.
        self.inner.procedures()
    }

    fn pushdown(&self) -> &PushdownProfile {
        self.inner.pushdown()
    }

    fn prelude(&self) -> &[AliasFn] {
        &self.prelude
    }

    fn version_support(&self, path: &Path) -> VersionSupport {
        self.inner
            .version_support(&Path::new(self.remap.path_in(path.as_str())))
    }

    fn plan_write(
        &self,
        path: &Path,
        verb: Verb,
        args: &RowBatch,
    ) -> Option<Result<Plan, CfsError>> {
        self.inner
            .plan_write(&Path::new(self.remap.path_in(path.as_str())), verb, args)
            .map(|res| res.map(|plan| self.remap.plan_out(plan)))
    }

    fn applier(&self) -> &dyn PlanApplier {
        // The binary's live commit path is the qfs-runtime interpreter over MountApplyDriver
        // (which rewrites per effect); this legacy sync seam delegates raw.
        self.inner.applier()
    }
}

/// A [`ReadDriver`] re-mounted under a custom segment: rewrites the scan's source id and
/// addressed path inward, then delegates the I/O to the wrapped driver.
pub struct MountReadDriver {
    remap: MountRemap,
    inner: Arc<dyn ReadDriver>,
}

impl MountReadDriver {
    /// Wrap `inner` under `remap` (share the remap built by [`MountDriver::new`]).
    #[must_use]
    pub fn new(remap: MountRemap, inner: Arc<dyn ReadDriver>) -> Self {
        Self { remap, inner }
    }
}

#[async_trait::async_trait]
impl ReadDriver for MountReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        self.inner.scan(&self.remap.scan_in(scan)).await
    }
}

/// An [`ApplyDriver`] re-mounted under a custom segment: rewrites every effect's target and
/// `Call` qualifier inward, then delegates the batch to the wrapped driver.
pub struct MountApplyDriver {
    remap: MountRemap,
    inner: Arc<dyn ApplyDriver>,
}

impl MountApplyDriver {
    /// Wrap `inner` under `remap` (share the remap built by [`MountDriver::new`]).
    #[must_use]
    pub fn new(remap: MountRemap, inner: Arc<dyn ApplyDriver>) -> Self {
        Self { remap, inner }
    }
}

#[async_trait::async_trait]
impl ApplyDriver for MountApplyDriver {
    async fn apply_batch(
        &self,
        kind: EffectKind,
        effects: &[EffectInput],
        cx: &ApplyCx,
    ) -> Vec<Result<EffectOutput, EffectError>> {
        let inward: Vec<EffectInput> = effects.iter().map(|e| self.remap.effect_in(e)).collect();
        self.inner
            .apply_batch(self.remap.kind_in(&kind), &inward, cx)
            .await
    }

    async fn apply_one(
        &self,
        effect: &EffectInput,
        cx: &ApplyCx,
    ) -> Result<EffectOutput, EffectError> {
        self.inner
            .apply_one(&self.remap.effect_in(effect), cx)
            .await
    }
}

#[cfg(test)]
mod tests {
    use qfs_core::{Archetype, Column, ColumnType, EffectNode, NodeId, Schema, Target, VfsPath};

    use super::*;

    /// An in-memory `/mail`-shaped fixture: path-keyed capabilities (only `/mail/inbox`
    /// is selectable), a `SEND → mail.send` prelude, and a `plan_write` that lowers to a
    /// `CALL mail.send` on an inner-mount target — enough surface to observe every rewrite.
    struct FixtureDriver {
        procs: Vec<ProcSig>,
        prelude: Vec<AliasFn>,
        pushdown: PushdownProfile,
    }

    impl FixtureDriver {
        fn new() -> Self {
            Self {
                procs: vec![ProcSig::new("send").irreversible(true)],
                prelude: vec![AliasFn::new("SEND", "mail.send")],
                pushdown: PushdownProfile::None,
            }
        }
    }

    impl Driver for FixtureDriver {
        fn mount(&self) -> &str {
            "/mail"
        }

        fn describe(&self, path: &Path) -> Result<NodeDesc, CfsError> {
            if path.as_str().starts_with("/mail") {
                Ok(NodeDesc::new(
                    Archetype::AppendLog,
                    Schema::new(vec![Column::new("subject", ColumnType::Text, false)]),
                ))
            } else {
                Err(CfsError::InvalidPath {
                    path: path.as_str().to_string(),
                    reason: "outside /mail",
                })
            }
        }

        fn capabilities(&self, path: &Path) -> Capabilities {
            if path.as_str() == "/mail/inbox" {
                Capabilities::from_verbs(&[Verb::Select])
            } else {
                Capabilities::none()
            }
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

        fn plan_write(
            &self,
            path: &Path,
            _verb: Verb,
            _args: &RowBatch,
        ) -> Option<Result<Plan, CfsError>> {
            // Lower to a CALL on the inner namespace so the test can watch the outbound
            // rewrite restore the outer id/path/qualifier.
            let node = EffectNode::new(
                NodeId(0),
                EffectKind::Call(ProcId::new("mail.send")),
                Target::new(DriverId::new("mail"), VfsPath::new(path.as_str())),
            );
            Some(Ok(Plan::leaf(node)))
        }

        fn applier(&self) -> &dyn PlanApplier {
            // Not reached in these tests — the adapter delegates this seam raw.
            unreachable!("fixture applier is never exercised")
        }
    }

    fn adapted() -> MountDriver {
        MountDriver::new("/mail2", Arc::new(FixtureDriver::new())).expect("valid mounts")
    }

    #[test]
    fn remap_requires_an_absolute_mount_and_a_bare_id() {
        assert!(MountRemap::new("mail2", "mail").is_err());
        assert!(MountRemap::new("/mail2", "").is_err());
        assert!(MountRemap::new("/mail2", "/mail").is_err());
    }

    #[test]
    fn prefix_rewrite_respects_segment_boundaries() {
        let remap = MountRemap::new("/mail2", "mail").expect("valid");
        // Inbound: the outer mount moves; deeper segments and coordinates ride along.
        assert_eq!(remap.path_in("/mail2"), "/mail");
        assert_eq!(remap.path_in("/mail2/inbox"), "/mail/inbox");
        assert_eq!(remap.path_in("/mail2@v1"), "/mail@v1");
        // A sibling mount that merely shares the text prefix is untouched.
        assert_eq!(remap.path_in("/mail2x/inbox"), "/mail2x/inbox");
        // Outbound is the exact inverse, with the same boundary rule.
        assert_eq!(remap.path_out("/mail/inbox"), "/mail2/inbox");
        assert_eq!(remap.path_out("/mailx/inbox"), "/mailx/inbox");
        // Non-path handles (draft ids) pass through both directions.
        assert_eq!(remap.path_in("id:draft-1"), "id:draft-1");
    }

    #[test]
    fn proc_qualifier_rewrites_both_directions() {
        let remap = MountRemap::new("/mail2", "mail").expect("valid");
        assert_eq!(remap.proc_out("mail.send"), "mail2.send");
        assert_eq!(remap.proc_in("mail2.send"), "mail.send");
        // A foreign qualifier is not touched.
        assert_eq!(remap.proc_out("git.merge"), "git.merge");
        // A qualifier that only shares the id's text prefix is not touched.
        assert_eq!(remap.proc_out("mailx.send"), "mailx.send");
    }

    #[test]
    fn adapter_id_derives_from_the_outer_mount() {
        let d = adapted();
        assert_eq!(d.mount(), "/mail2");
        assert_eq!(d.id(), DriverId::new("mail2"));
    }

    #[test]
    fn describe_and_capabilities_rewrite_the_inbound_path() {
        let d = adapted();
        // The fixture only answers under /mail — reaching it through /mail2 proves the rewrite.
        assert!(d.describe(&Path::new("/mail2/inbox")).is_ok());
        assert!(d
            .capabilities(&Path::new("/mail2/inbox"))
            .allows(Verb::Select));
        // The inner mount is NOT addressable through the adapter under its native name:
        // /mail/... enters the fixture unchanged and misses its /mail/inbox capability key
        // only when the outer prefix is absent.
        assert!(!d
            .capabilities(&Path::new("/mail2/other"))
            .allows(Verb::Select));
    }

    #[test]
    fn prelude_is_requalified_to_the_outer_id() {
        let d = adapted();
        assert_eq!(d.prelude().len(), 1);
        assert_eq!(d.prelude()[0].name, "SEND");
        assert_eq!(d.prelude()[0].desugars_to, "mail2.send");
    }

    #[test]
    fn plan_write_rewrites_targets_and_call_qualifiers_outbound() {
        let d = adapted();
        let plan = d
            .plan_write(
                &Path::new("/mail2/drafts"),
                Verb::Insert,
                &RowBatch::default(),
            )
            .expect("fixture lowers writes")
            .expect("lowering succeeds");
        let node = &plan.nodes[0];
        assert_eq!(node.target.driver, DriverId::new("mail2"));
        assert_eq!(node.target.path.as_str(), "/mail2/drafts");
        match &node.kind {
            EffectKind::Call(proc) => assert_eq!(proc.0, "mail2.send"),
            other => panic!("expected a CALL node, got {other:?}"),
        }
    }

    /// A read fixture that asserts the scan it receives was rewritten inward.
    struct AssertingRead;

    #[async_trait::async_trait]
    impl ReadDriver for AssertingRead {
        async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
            assert_eq!(scan.source.as_str(), "mail");
            assert_eq!(scan.path, "/mail/inbox");
            Ok(RowBatch::default())
        }
    }

    #[tokio::test]
    async fn read_adapter_rewrites_source_and_path_inward() {
        let remap = MountRemap::new("/mail2", "mail").expect("valid");
        let read = MountReadDriver::new(remap, Arc::new(AssertingRead));
        let scan = ScanNode {
            source: SourceId::new("mail2"),
            path: "/mail2/inbox".to_string(),
            pushed: qfs_pushdown::PushedQuery::default(),
            schema: Schema::new(vec![]),
        };
        let rows = read.scan(&scan).await.expect("scan succeeds");
        assert_eq!(rows.rows.len(), 0);
    }

    /// An apply fixture that asserts the effect it receives was rewritten inward.
    struct AssertingApply;

    #[async_trait::async_trait]
    impl ApplyDriver for AssertingApply {
        async fn apply_one(
            &self,
            effect: &EffectInput,
            _cx: &ApplyCx,
        ) -> Result<EffectOutput, EffectError> {
            assert_eq!(effect.target.driver, DriverId::new("mail"));
            assert_eq!(effect.target.path.as_str(), "/mail/drafts/1");
            match &effect.kind {
                EffectKind::Call(proc) => assert_eq!(proc.0, "mail.send"),
                other => panic!("expected a CALL effect, got {other:?}"),
            }
            Ok(EffectOutput::new(effect.id, 1))
        }
    }

    #[tokio::test]
    async fn apply_adapter_rewrites_targets_and_qualifiers_inward() {
        let remap = MountRemap::new("/mail2", "mail").expect("valid");
        let apply = MountApplyDriver::new(remap, Arc::new(AssertingApply));
        let node = EffectNode::new(
            NodeId(7),
            EffectKind::Call(ProcId::new("mail2.send")),
            Target::new(DriverId::new("mail2"), VfsPath::new("/mail2/drafts/1")),
        );
        let effect = EffectInput::from_node(&node);
        let out = apply
            .apply_one(&effect, &ApplyCx::default())
            .await
            .expect("apply succeeds");
        assert_eq!(out.affected, 1);
        // The batched entrypoint shares the same rewrite (kind + every input).
        let outs = apply
            .apply_batch(
                EffectKind::Call(ProcId::new("mail2.send")),
                std::slice::from_ref(&effect),
                &ApplyCx::default(),
            )
            .await;
        assert_eq!(outs.len(), 1);
        assert!(outs[0].is_ok());
    }
}
