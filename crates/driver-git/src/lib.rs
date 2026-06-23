//! `cfs-driver-git` — the **git driver** (RFD-0001 §5, E4 t26): the canonical proof that ONE
//! [`Driver`] exposes **all four archetypes** on different sub-paths, over the **local git
//! object database** (loose objects + refs + reflog) — NOT the GitHub HTTP API (that is the
//! separate `github` object-graph driver). Mounted at `/git/<repo>[@<ref>]/<rest>`:
//!
//! - **BlobFs** (Blob, [`blobfs`]): `ls`/read trees+blobs at any `@<ref>` (the §4 temporal
//!   coordinate); blob bytes feed the **t15 codec registry** so `DECODE json|yaml|toml|csv|
//!   md+frontmatter` works on committed files.
//! - **Commits / Changes / Blame / Refs / Tags** (Relational, [`dto`]): owned row DTOs with
//!   `WHERE/ORDER BY/LIMIT/JOIN`; revwalk-expressible predicates push down (a `LIMIT` bounds the
//!   walk), the rest stay a **truthful residual** (the t20 lesson — never wrong rows).
//! - **Reflog** (Append/log): a tail read — the recovery oracle.
//!
//! **Writes are pure plans** ([`planner`], the purity invariant, RFD §3): a write builds a DAG of
//! [`GitEffect`]s and applies **nothing** until `COMMIT` drives [`GitApplier`]. Object writes are
//! content-addressed → idempotent, reversible (GC-able). Ref moves are **compare-and-swap on the
//! old oid** (optimistic concurrency via `@version`): a stale old-oid is **rejected, never
//! clobbered**; a forced move is flagged but **reflog-recoverable** ([`GitApplier::recover_ref`]).
//!
//! ## The COMMIT keyword clash (the hard part) — resolved
//! A git "commit" must NOT touch the frozen plan keyword `COMMIT`. It is modelled strictly as
//! **`INSERT INTO /git/<repo>/commits`** ([`planner::plan_insert_commit`]); the commits node's
//! capability set is `{SELECT, INSERT}` (NO update/remove), and the schema/DESCRIBE document this
//! so the AI never emits `COMMIT` to create a commit. `COMMIT` remains exclusively the plan-apply
//! verb the interpreter runs.
//!
//! ## merge/rebase/checkout/tag = `CALL git.*` procedures
//! Irreducible transitions are namespaced procedures ([`procedures`]) returning **pure plan
//! DAGs**. `merge`/`rebase` compute the result tree DURING planning (in-memory three-way merge)
//! and surface a conflict as a typed plan-build error ([`GitError::MergeConflict`]) in PREVIEW
//! with **ZERO** effects — never a half-applied mutation. `git.merge` ≠ `github.merge` (namespace).
//!
//! ## Capability gating at PARSE time
//! [`GitDriver::capabilities`] is per-node: `UPDATE /commits` is rejected **structurally** by the
//! resolve-time gate ([`cfs_driver::check_capability`]) before a Plan exists.
//!
//! ## No vendor leak (RFD §9) + the in-house object reader (ADR-0003)
//! Owned DTOs only — no `gix` type crosses the boundary. Per **ADR-0003** the object reader is
//! **in-house** (pure-Rust DEFLATE inflate + SHA-1 content addressing + `<type> <len>\0<payload>`
//! framing), zero new dependency crates, wasm-clean, differentially checked against real `git`
//! fixture output. `gix` was rejected on the same footprint/offline/wasm grounds ADR-0002 rejected
//! DuckDB. git's SHA-1/SHA-256 object hashing is separate from the objstore/slack HMAC and the
//! carry-over `cfs-crypto-core` (t26 does not consume it).
//!
//! ## Named parks (deferred per the ticket)
//! - **Pack-delta resolution / partial clone / submodules / LFS / remote transport** — out of
//!   scope; the fixture keeps referenced objects loose (ADR-0003). The `ObjectDb` seam admits a
//!   future `gix`/pack backend without a rewrite.
//! - **Nested trees** — the E0 tree builder + three-way merge operate on a flat tree; nested
//!   subtree write/merge is a named park.
//! - **Deep-history blame engine** — [`blobfs::blame`] does bounded last-touched attribution; a
//!   full per-line blame over deep history is parked (RFD §6 bound-the-work).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod blobfs;
pub mod dto;
mod effect;
mod error;
mod inflate;
pub mod objectdb;
pub mod path;
pub mod planner;
pub mod procedures;
pub mod relational;
pub mod repo;
mod sha1;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use cfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
use cfs_plan::PlanApplier;
use cfs_runtime::PlanApplierBridge;

pub use applier::{object_oid, GitApplier, RepoStore};
pub use dto::{BlameRow, ChangeRow, CommitRow, RefRow, ReflogRow};
pub use effect::GitEffect;
pub use error::GitError;
pub use objectdb::{
    frame_and_id, parse_commit, parse_tag, parse_tree, serialize_tree, Commit, LooseObjectDb,
    ObjectDb, ObjectKind, Oid, RawObject, Tag, Tree, TreeEntry,
};
pub use path::{GitNode, GitPath, MOUNT};
pub use planner::{
    plan_checkout, plan_insert_commit, plan_merge, plan_rebase, plan_tag, plan_update_ref,
    CommitInput, CommitPlan,
};
pub use repo::{RefTarget, ReflogEntry, Repo, RepoResolver};

/// The git least-privilege scope labels (RFD §10). The local object model needs NO network
/// token — a deliberate security win. These are documentation labels only, never a credential.
pub const GIT_READ_SCOPE: &str = "git:read-objects";
/// The git write scope label (object + ref writes are local).
pub const GIT_WRITE_SCOPE: &str = "git:write-objects git:update-ref";

/// The git driver (RFD §5). Owns the [`RepoResolver`] (the read path) + the [`GitApplier`] (the
/// COMMIT apply path) + the declared pushdown profile + the `CALL git.*` procedure declarations.
pub struct GitDriver {
    repos: RepoResolver,
    applier: GitApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl GitDriver {
    /// Build a git driver over a repository resolver and an apply-leg store.
    #[must_use]
    pub fn new(repos: RepoResolver, applier: GitApplier) -> Self {
        Self {
            repos,
            applier,
            // The relational history nodes push a ref-range / LIMIT down to the revwalk and keep
            // the rest as a truthful residual the engine re-filters (the t20 lesson). WHERE on
            // the walked-from ref + LIMIT are native; ORDER BY time is the natural revwalk order;
            // everything richer stays residual.
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: true,
                limit: true,
                order: true,
                join: false,
                aggregate: false,
                distinct: false,
                group_by: false,
            },
            procs: procedures::git_procedures(),
        }
    }

    /// Borrow the repository resolver (the read path).
    #[must_use]
    pub fn repos(&self) -> &RepoResolver {
        &self.repos
    }

    /// Borrow the synchronous applier (to build the runtime bridge / drive a direct apply / call
    /// the recovery helper).
    #[must_use]
    pub fn git_applier(&self) -> &GitApplier {
        &self.applier
    }

    /// The per-node capability set (RFD §5), gated at parse time:
    /// - **commits** → `{SELECT, INSERT}` (INSERT = make a commit; **no** update/remove — so
    ///   `UPDATE /commits` is rejected structurally, the keyword-clash + capability requirement).
    /// - **changes / blame** → `{SELECT}` (derived read-only views).
    /// - **refs / tags** → `{SELECT, UPDATE}` (move/create a ref via CAS).
    /// - **reflog** → `{SELECT}` (append-log tail read).
    /// - **BlobFs** (tree/blob/root) → `{SELECT, LS}` (read-only versioned FS; writes go through
    ///   `INSERT INTO /commits`, not a blob write).
    fn caps_for(&self, path: &Path) -> Capabilities {
        let Ok(gp) = GitPath::parse(path.as_str()) else {
            return Capabilities::none();
        };
        if !self.repos.has_repo(&gp.repo) {
            return Capabilities::none();
        }
        match gp.node {
            GitNode::Commits => Capabilities::from_verbs(&[Verb::Select, Verb::Insert]),
            GitNode::Changes | GitNode::Blame { .. } | GitNode::Reflog => {
                Capabilities::from_verbs(&[Verb::Select])
            }
            GitNode::Refs | GitNode::Tags => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Update])
            }
            GitNode::Blob { .. } | GitNode::Root => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Ls])
            }
        }
    }

    /// The archetype + typed schema for a node (the DESCRIBE output).
    fn node_desc(&self, gp: &GitPath) -> NodeDesc {
        match &gp.node {
            GitNode::Commits => NodeDesc::new(Archetype::RelationalTable, CommitRow::schema()),
            GitNode::Changes => NodeDesc::new(Archetype::RelationalTable, ChangeRow::schema()),
            GitNode::Blame { .. } => NodeDesc::new(Archetype::RelationalTable, BlameRow::schema()),
            GitNode::Refs | GitNode::Tags => {
                NodeDesc::new(Archetype::RelationalTable, RefRow::schema())
            }
            GitNode::Reflog => NodeDesc::new(Archetype::AppendLog, ReflogRow::schema()),
            GitNode::Blob { .. } | GitNode::Root => {
                NodeDesc::new(Archetype::BlobNamespace, dto::blob_listing_schema())
            }
        }
    }
}

impl Driver for GitDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, cfs_driver::CfsError> {
        let gp = GitPath::parse(path.as_str()).map_err(|e| e.into_cfs(path.as_str()))?;
        if !self.repos.has_repo(&gp.repo) {
            return Err(GitError::UnknownRepo {
                repo: gp.repo.clone(),
            }
            .into_cfs(path.as_str()));
        }
        Ok(self.node_desc(&gp))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        self.caps_for(path)
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn version_support(&self, path: &Path) -> VersionSupport {
        // Every git node is fully version-addressable by `@<ref>` (the §4 temporal coordinate):
        // commits, trees, blobs, and refs all read AS OF a ref/sha. Reflog is a snapshot tail.
        match GitPath::parse(path.as_str()) {
            Ok(gp) if self.repos.has_repo(&gp.repo) => match gp.node {
                GitNode::Reflog => VersionSupport::Snapshot,
                _ => VersionSupport::Versioned,
            },
            _ => VersionSupport::None,
        }
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`GitDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding the
/// async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id `git`, so a
/// plan over `/git` executes end-to-end through the t10 interpreter — **the locked driver
/// pattern**.
#[must_use]
pub fn git_apply_driver(driver: &GitDriver) -> PlanApplierBridge<GitApplier> {
    PlanApplierBridge::new(Arc::new(driver.git_applier().clone()))
}
