//! `cfs-driver-github` — the **GitHub object-graph + workflow `Driver`** (RFD-0001 §5, E4 t24).
//! It mounts GitHub under `/github/{owner}/{repo}/...` as a path tree of eight namespaces —
//! `issues`, `pulls`, `comments`, `reviews`, `runs`, `releases`, `files`, `branches` — each an
//! [`Archetype::ObjectGraphWorkflow`] node with a typed [`Schema`] powering `DESCRIBE`. It
//! implements the t13 [`cfs_driver::Driver`] contract and reuses the t18 reusable-HTTP-seam
//! *shape* — Bearer/PAT injection, RFC-5988 Link-header pagination, 429/`Retry-After` bounded retry
//! on idempotent GETs only — over the **shared `cfs_http_core` HTTP DTOs + the single redaction
//! authority** through a local [`HttpTransport`] seam (a structural twin of t18's `HttpClient`).
//! There is **no hand-rolled HTTP DTO** (the t19 redaction-drift token leak stays closed) and the
//! driver does **not** depend on `cfs-driver-http` as a crate: a `cfs-runtime` consumer must stay a
//! leaf (the dep-direction confinement test), so the reqwest wire impl rides the transport seam the
//! way gdrive rides `cfs-google-auth`'s `HttpExchange` (the production wire is parked for t38).
//!
//! ## Surface
//! - [`GitHubDriver`] — the introspective `Driver`: `mount()` = `/github`, the
//!   [`Archetype::ObjectGraphWorkflow`] per-node archetype + the per-namespace [`Schema`],
//!   **node-keyed** capabilities (a runs node is read-only; an issue node admits
//!   `SELECT/INSERT/UPDATE`; a comment node admits `INSERT/REMOVE`; …), the
//!   `merge`/`dispatch`/`review` procedures, and `Partial { where_, limit }` pushdown.
//! - [`GitHubApplier`] — the synchronous apply leg `applier()` returns and the
//!   [`cfs_runtime::SharedApplier`] the bridge drives under `COMMIT`.
//! - [`github_apply_driver`] — wraps the applier in a [`cfs_runtime::PlanApplierBridge`] ready to
//!   `register` into a `DriverRegistry` under the driver id `github`.
//!
//! ## Universal CRUD + the irreducible `CALL` procedures (RFD §3)
//! Universal verbs map onto REST: `SELECT` (list/get + Link pagination), `INSERT` (open issue/PR,
//! post comment, create release/branch), `UPDATE … SET state='closed'`/title/body/labels → PATCH,
//! `REMOVE` (delete comment/release/branch). The three state transitions GitHub has no universal
//! verb for are `CALL github.merge/dispatch/review`, each building an `Effect::Call` HTTP-call
//! node; `merge`/`dispatch` are marked **irreversible**. `merge` uses optimistic concurrency on
//! the PR head SHA (`sha=>`); `dispatch` returns 204 with no run id and resolves a *queued* status
//! (a follow-up `SELECT … FROM runs` polls the run) rather than fabricating an id; comment POSTs
//! are **at-least-once** (never silently retried).
//!
//! ## Pushdown is TRUTHFUL with a residual (the t20 lesson, non-negotiable)
//! [`pushdown::build_params`] lowers a `WHERE` on `issues`/`pulls` into GitHub list params
//! (`state`/`assignee` exact; `labels` membership pre-filter). A term is dropped from the residual
//! **only** when the param means *exactly* the SQL predicate; every looser term keeps the exact
//! predicate as **residual** so the engine re-filters locally — over-fetch then filter, never
//! wrong rows (RFD §6).
//!
//! ## Token safety (RFD §10)
//! The PAT is a [`cfs_secrets::Secret`] read **only** at plan-apply time, written into an
//! `Authorization: Bearer …` header the redacting [`cfs_http_core::HttpRequest`] `Debug` hides, and
//! is **never** logged, never in a DTO/error, never in a serialized plan (a planted-canary test
//! asserts this).
//!
//! ## The `files`/`branches` boundary (kept strictly distinct from the t26 git driver)
//! Here `files` is a **read-only GitHub-API content-metadata view** (path + sha + size + type) and
//! `branches` is **branch-ref metadata** (read + create/delete a ref). Neither is a working tree:
//! there is no blob content, no commit history, no mutable refs walk. The versioned-blob FS,
//! `commits` relational history, and mutable `refs` belong to the **git** driver (t26).
//!
//! ## No vendor leak (RFD §9)
//! GitHub JSON is translated into owned DTOs ([`IssueDto`]…[`FileMetaDto`]) at the [`client`]
//! boundary; the `Driver` surface and the `Plan` carry **zero** octocrab/vendor types (a
//! DTO-boundary test asserts no vendor type in any public signature). The HTTP client is behind the
//! mockable [`GitHubClient`] trait so it mocks in tests (no live GitHub, no network).
//!
//! ## Named parks (deferred)
//! - **Live GitHub API + live token — surface present, no live test (t38).** Every test drives the
//!   mocked [`GitHubClient`] seam; the real [`RestGitHubClient`] over the t18 reqwest client is
//!   construction-checked but never sent over a socket here (live E2E parked for t38).
//! - **`dispatch` run-id polling — modelled as a queued resolution.** `dispatch` returns 204; the
//!   effect resolves "queued" and a follow-up `SELECT … FROM runs` is the id-resolution path (no
//!   fabricated id).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod client;
pub mod dto;
mod effect;
mod error;
pub mod path;
pub mod procs;
pub mod pushdown;
pub mod read;
pub mod schema;

use std::sync::Arc;

use cfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use cfs_plan::PlanApplier;
use cfs_runtime::PlanApplierBridge;

pub use applier::GitHubApplier;
pub use client::{
    GitHubClient, HttpTransport, MockGitHubClient, RecordedCall, RestGitHubClient, TransportError,
};
pub use dto::{
    BranchDto, CommentDto, FileMetaDto, IssueDto, PullDto, ReleaseDto, ReviewDto, RunDto,
};
pub use effect::GitHubEffect;
pub use error::GitHubError;
pub use path::{GitHubPath, Namespace, MOUNT};
pub use read::ReadPlan;
pub use schema::schema_for;

/// The GitHub driver (RFD §5). Owns the synchronous [`GitHubApplier`] the contract returns from
/// `applier()`, plus the declared procedures and pushdown profile. Construct with
/// [`GitHubDriver::new`], injecting the [`GitHubClient`] (auth is injected there at construction —
/// the real client resolves the PAT from the secret store; never on the contract surface).
pub struct GitHubDriver {
    applier: GitHubApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl GitHubDriver {
    /// Build a GitHub driver over `client`. In production `client` is a [`RestGitHubClient`]
    /// wrapping the t18 HTTP client + the secret store; in tests it is a [`MockGitHubClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GitHubClient>) -> Self {
        Self {
            applier: GitHubApplier::new(client),
            // GitHub's list endpoints filter on `state`/`labels`/`assignee` and cap with
            // `per_page` (a `LIMIT`-shaped bound); ordering/projection/joins stay local. The
            // residual keeps exact correctness (RFD §6).
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
            procs: procs::procedures(),
        }
    }

    /// Borrow the synchronous applier (e.g. to drive a `cfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn github_applier(&self) -> &GitHubApplier {
        &self.applier
    }

    /// The node-keyed capability set (RFD §5), gating verbs at parse time. The effective namespace
    /// (sub-collection if present, else top-level) decides the verb set:
    /// - `issues`     → `SELECT|INSERT|UPDATE` (list/open/close-edit; no REMOVE — issues are
    ///   closed, never deleted).
    /// - `pulls`      → `SELECT|INSERT|UPDATE` (list/open/close-edit; merge/review via CALL).
    /// - `comments`   → `SELECT|INSERT|REMOVE` (list/post/delete).
    /// - `releases`   → `SELECT|INSERT|REMOVE` (list/create/delete).
    /// - `branches`   → `SELECT|INSERT|REMOVE` (list/create-ref/delete-ref).
    /// - `reviews`    → `SELECT` (submitting a review is `CALL github.review`).
    /// - `runs`       → `SELECT` (read-only; trigger is `CALL github.dispatch`).
    /// - `files`      → `SELECT` (read-only API content-metadata view).
    /// - the repo root / an unknown node → the empty set (every verb rejected at the gate).
    fn caps_for(&self, path: &Path) -> Capabilities {
        let Ok(parsed) = GitHubPath::parse(path) else {
            return Capabilities::none();
        };
        let Some(ns) = parsed.effective_namespace() else {
            return Capabilities::none();
        };
        match ns {
            Namespace::Issues | Namespace::Pulls => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Insert, Verb::Update])
            }
            Namespace::Comments | Namespace::Releases | Namespace::Branches => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Insert, Verb::Remove])
            }
            Namespace::Reviews | Namespace::Runs | Namespace::Files => {
                Capabilities::from_verbs(&[Verb::Select])
            }
        }
    }
}

impl Driver for GitHubDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, cfs_driver::CfsError> {
        // Every /github node is the object-graph+workflow archetype; its relation is the
        // effective namespace's canonical schema. Pure: builds data, no I/O. A path that names no
        // namespace (the bare repo root) is not a describable collection — surface an honest
        // structured InvalidPath rather than inventing an empty schema.
        let parsed = GitHubPath::parse(path).map_err(|_| cfs_driver::CfsError::InvalidPath {
            path: path.as_str().to_string(),
            reason: "not a /github/{owner}/{repo}/<namespace> node",
        })?;
        let ns = parsed
            .effective_namespace()
            .ok_or_else(|| cfs_driver::CfsError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "the GitHub repo root is not a describable collection",
            })?;
        Ok(NodeDesc::new(
            Archetype::ObjectGraphWorkflow,
            schema::schema_for(ns),
        ))
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

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`GitHubDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding
/// the async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id
/// `github`. A plan routed to `/github` then executes end-to-end through the t10 interpreter,
/// which dispatches each effect to this bridge.
#[must_use]
pub fn github_apply_driver(driver: &GitHubDriver) -> PlanApplierBridge<GitHubApplier> {
    PlanApplierBridge::new(Arc::new(driver.github_applier().clone()))
}

#[cfg(test)]
mod tests;
