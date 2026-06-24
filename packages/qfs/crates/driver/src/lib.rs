//! `qfs-driver` — the driver contract (RFD-0001 §5).
//!
//! A driver declares its namespace, per-node archetype + typed [`Schema`], capabilities,
//! procedures, pushdown ability, prelude, and `@version` support — and that declaration
//! is everything the engine and the AI need. This crate defines the **consumer-side
//! narrow trait** every E4 driver fills, plus the owned-DTO conventions that keep vendor
//! SDK types from crossing the boundary (§9, boundary B3 — the direct generalisation of
//! the Go `internal/gmail` SDK quarantine).
//!
//! ## Purity invariant at the type level (fidelity guard G3, boundary B4)
//! The **introspective** half of [`Driver`] (`describe`/`capabilities`/`procedures`/
//! `pushdown`/`prelude`/`version_support`) returns **data** — owned DTOs and the typed
//! [`Schema`]. **No introspective method takes `&mut self`, returns a future, or
//! performs I/O.** The lone impure seam is [`Driver::applier`], which hands back the
//! [`PlanApplier`] (t09) that the runtime invokes **only under `COMMIT`** — so `PREVIEW`
//! and CI dry-runs never touch the World. The in-crate test
//! [`tests::fixture_driver_introspection_is_pure`] proves the introspective half by
//! exercising a no-I/O fixture driver.
//!
//! ## Schema is the canonical typed model (RFD §5)
//! `describe` returns a [`NodeDesc`] = archetype tag + `qfs_types::Schema` (the canonical
//! typed, vendor-free schema from t05). The driver crate does **not** redefine an untyped
//! schema; it trades in the one workspace schema so `DESCRIBE` and type-checking agree.
//!
//! ## Path boundary (the pushdown/effect surface)
//! The contract speaks [`Path`]; the effect substrate speaks [`qfs_plan::VfsPath`]. The
//! explicit lossless adapter lives on [`Path`] ([`Path::to_vfs`] / [`Path::from_vfs`] /
//! [`Path::try_from_vfs`]); a driver crosses the boundary through it, never through a
//! vendor type.
//!
//! ## Shared primitives
//! This crate owns [`CfsError`] and [`Path`] (decision D1 — see [`error`]) because the
//! trait signatures need them and the acyclic spine forbids reaching up into `qfs-core`.
//!
//! ## wasm-friendliness (boundary guard B7)
//! No threads, no `std::fs`, no sockets. I/O lives in (future) driver *impls* behind the
//! `applier()` seam, never in this contract crate.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod error;
mod path;

pub use error::CfsError;
pub use path::Path;

use qfs_plan::{Plan, PlanApplier};
use qfs_types::{ColumnType, DriverId, Schema};
use serde::Serialize;

/// How a node maps onto qfs's uniform model (RFD-0001 §5, "Four archetypes").
///
/// A single driver may expose multiple archetypes on different sub-paths (git is all
/// three: versioned-blob FS, relational history, mutable pointers) — so the archetype is
/// **per-node** (carried in [`NodeDesc`], path-keyed via [`Driver::describe`]), not
/// driver-global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Archetype {
    /// `ls cp mv rm` — local FS, S3/R2, Drive, repo files, Slack files.
    BlobNamespace,
    /// `SELECT JOIN INSERT UPDATE` — SQL DBs, D1, Notion DB.
    RelationalTable,
    /// `SELECT(tail) INSERT(append)` — Slack, mail, CF Queues, comments, webhooks.
    AppendLog,
    /// CRUD + `CALL` procs — GitHub, Linear, K8s.
    ObjectGraphWorkflow,
}

/// A universal verb (RFD-0001 §3/§5). A **closed set** mirroring the frozen core verbs;
/// a new backend adds **zero** variants — it declares which of these a node supports via
/// [`Capabilities`]. Unsupported verbs are rejected at parse/resolve time with a
/// structured [`CfsError::UnsupportedVerb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verb {
    /// `SELECT` — read.
    Select,
    /// `INSERT INTO`.
    Insert,
    /// `UPSERT INTO` — idempotent create-or-update (retry-safe, §6).
    Upsert,
    /// `UPDATE`.
    Update,
    /// `REMOVE`.
    Remove,
    /// `LS` — list a blob namespace.
    Ls,
    /// `CP` — copy.
    Cp,
    /// `MV` — move.
    Mv,
    /// `RM` — delete a blob/object.
    Rm,
}

impl Verb {
    /// A short, stable label for the structured error, golden snapshots, and AI feedback.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Verb::Select => "SELECT",
            Verb::Insert => "INSERT",
            Verb::Upsert => "UPSERT",
            Verb::Update => "UPDATE",
            Verb::Remove => "REMOVE",
            Verb::Ls => "LS",
            Verb::Cp => "CP",
            Verb::Mv => "MV",
            Verb::Rm => "RM",
        }
    }
}

/// The set of universal verbs a node supports (RFD-0001 §5). Unsupported verbs are
/// rejected **at parse time** with a structured [`CfsError`] — important for AI.
///
/// Capabilities are **per-node** (path-dependent): a single driver mixes archetypes on
/// sub-paths, so a driver returns these from [`Driver::capabilities`] keyed on the path,
/// not as a driver-global constant. The fields are owned booleans — no vendor type, no
/// I/O.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// Supports `SELECT` (read).
    pub select: bool,
    /// Supports `INSERT INTO`.
    pub insert: bool,
    /// Supports `UPSERT INTO`.
    pub upsert: bool,
    /// Supports `UPDATE`.
    pub update: bool,
    /// Supports `REMOVE`.
    pub remove: bool,
    /// Supports `LS` (list a blob namespace).
    pub ls: bool,
    /// Supports `CP` (copy).
    pub cp: bool,
    /// Supports `MV` (move).
    pub mv: bool,
    /// Supports `RM` (delete a blob/object).
    pub rm: bool,
}

impl Capabilities {
    /// The empty capability set — every verb denied. Out-of-crate driver impls (E4)
    /// build from here with the chainable setters, since the struct is
    /// `#[non_exhaustive]` and a struct literal is therefore unavailable (E0639).
    ///
    /// `Capabilities::none()` is an alias for [`Capabilities::default()`]; it reads
    /// better as the start of a builder chain (`Capabilities::none().select().insert()`).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            select: false,
            insert: false,
            upsert: false,
            update: false,
            remove: false,
            ls: false,
            cp: false,
            mv: false,
            rm: false,
        }
    }

    /// Build a capability set from a slice of supported [`Verb`]s — the declarative
    /// form a driver uses when its node's verb set is known up front. Verbs not listed
    /// stay denied.
    #[must_use]
    pub fn from_verbs(verbs: &[Verb]) -> Self {
        let mut caps = Self::none();
        for &verb in verbs {
            caps = caps.with(verb);
        }
        caps
    }

    /// Set (enable) a single [`Verb`] in this set — the verb-keyed builder step used by
    /// [`Capabilities::from_verbs`] and available to driver authors that branch on a
    /// [`Verb`] value rather than a named field.
    #[must_use]
    pub const fn with(mut self, verb: Verb) -> Self {
        match verb {
            Verb::Select => self.select = true,
            Verb::Insert => self.insert = true,
            Verb::Upsert => self.upsert = true,
            Verb::Update => self.update = true,
            Verb::Remove => self.remove = true,
            Verb::Ls => self.ls = true,
            Verb::Cp => self.cp = true,
            Verb::Mv => self.mv = true,
            Verb::Rm => self.rm = true,
        }
        self
    }

    /// Builder: enable `SELECT`.
    #[must_use]
    pub const fn select(self) -> Self {
        self.with(Verb::Select)
    }

    /// Builder: enable `INSERT`.
    #[must_use]
    pub const fn insert(self) -> Self {
        self.with(Verb::Insert)
    }

    /// Builder: enable `UPSERT`.
    #[must_use]
    pub const fn upsert(self) -> Self {
        self.with(Verb::Upsert)
    }

    /// Builder: enable `UPDATE`.
    #[must_use]
    pub const fn update(self) -> Self {
        self.with(Verb::Update)
    }

    /// Builder: enable `REMOVE`.
    #[must_use]
    pub const fn remove(self) -> Self {
        self.with(Verb::Remove)
    }

    /// Builder: enable `LS`.
    #[must_use]
    pub const fn ls(self) -> Self {
        self.with(Verb::Ls)
    }

    /// Builder: enable `CP`.
    #[must_use]
    pub const fn cp(self) -> Self {
        self.with(Verb::Cp)
    }

    /// Builder: enable `MV`.
    #[must_use]
    pub const fn mv(self) -> Self {
        self.with(Verb::Mv)
    }

    /// Builder: enable `RM`.
    #[must_use]
    pub const fn rm(self) -> Self {
        self.with(Verb::Rm)
    }

    /// The canonical, declaration-ordered list of every [`Verb`]. The single source of
    /// truth shared by [`Capabilities::supported_labels`] and the verb/capability tie
    /// test, so a new verb cannot drift out of sync with the capability flags.
    pub(crate) const ALL_VERBS: [Verb; 9] = [
        Verb::Select,
        Verb::Insert,
        Verb::Upsert,
        Verb::Update,
        Verb::Remove,
        Verb::Ls,
        Verb::Cp,
        Verb::Mv,
        Verb::Rm,
    ];

    /// Whether the given [`Verb`] is supported at this node.
    #[must_use]
    pub const fn allows(&self, verb: Verb) -> bool {
        match verb {
            Verb::Select => self.select,
            Verb::Insert => self.insert,
            Verb::Upsert => self.upsert,
            Verb::Update => self.update,
            Verb::Remove => self.remove,
            Verb::Ls => self.ls,
            Verb::Cp => self.cp,
            Verb::Mv => self.mv,
            Verb::Rm => self.rm,
        }
    }

    /// The supported verbs as stable labels, in canonical order — the `supported:` set a
    /// structured [`CfsError::UnsupportedVerb`] carries for AI recovery.
    #[must_use]
    pub fn supported_labels(&self) -> Vec<&'static str> {
        Self::ALL_VERBS
            .iter()
            .filter(|v| self.allows(**v))
            .map(|v| v.label())
            .collect()
    }
}

/// The archetype + typed [`Schema`] of a node — the output of `DESCRIBE` (RFD §5).
///
/// This is the reconciliation point between the driver contract and the canonical type
/// model: the schema is `qfs_types::Schema` (typed columns from t05), and the archetype
/// is the separate uniform-model tag. Owned data only; `Serialize` for `-json DESCRIBE`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct NodeDesc {
    /// How this node maps onto the uniform model.
    pub archetype: Archetype,
    /// The node's typed columns (the canonical `qfs_types::Schema`; powers type-checking).
    pub schema: Schema,
}

impl NodeDesc {
    /// Construct a node description. Provided because the struct is `#[non_exhaustive]`,
    /// so out-of-crate driver impls (E4) cannot use a struct literal.
    #[must_use]
    pub fn new(archetype: Archetype, schema: Schema) -> Self {
        Self { archetype, schema }
    }
}

/// One declared parameter of a [`ProcSig`]. Owned name + type; no vendor type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Param {
    /// The parameter name, e.g. `to`, `subject`.
    pub name: String,
    /// The parameter's type, drawn from the canonical type model (t05).
    pub ty: ColumnType,
}

impl Param {
    /// Construct a parameter declaration.
    #[must_use]
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// Declaration of a domain procedure callable via `CALL driver.action(...)`
/// (RFD-0001 §3/§5 — the irreducible state transitions).
///
/// `CALL` only resolves procedures a driver declares (capability). `irreversible` lets
/// `PREVIEW` warn and `POLICY` block (RFD §6/§10); `requires_scopes` lets the server
/// `POLICY` reason about blast radius (RFD §10) — a hint only, never a credential. Owned
/// data only.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ProcSig {
    /// The unqualified action name, e.g. `send`, `merge`. Qualified at the call site by
    /// the driver mount (`mail.send` vs `git.merge`).
    pub name: String,
    /// The declared parameters, in order.
    pub params: Vec<Param>,
    /// Whether applying this procedure cannot be undone (e.g. `mail.send`). Surfaced per
    /// `Call` effect node so `PREVIEW` can warn (RFD §6/§10).
    pub irreversible: bool,
    /// The procedure's result schema, if it returns rows (e.g. a search proc).
    pub returns: Option<Schema>,
    /// Least-privilege scope hints the server `POLICY` reasons over (RFD §10). Owned
    /// labels only — **never** a token or credential.
    pub requires_scopes: Vec<String>,
}

impl ProcSig {
    /// Construct a minimal, reversible procedure with no params, no return, no scopes.
    /// Use the builders to add detail.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            params: Vec::new(),
            irreversible: false,
            returns: None,
            requires_scopes: Vec::new(),
        }
    }

    /// Builder: set the declared parameters.
    #[must_use]
    pub fn with_params(mut self, params: Vec<Param>) -> Self {
        self.params = params;
        self
    }

    /// Builder: mark this procedure irreversible (e.g. `mail.send`).
    #[must_use]
    pub fn irreversible(mut self, yes: bool) -> Self {
        self.irreversible = yes;
        self
    }

    /// Builder: set the result schema for a row-returning procedure.
    #[must_use]
    pub fn returns(mut self, schema: Schema) -> Self {
        self.returns = Some(schema);
        self
    }

    /// Builder: attach least-privilege scope hints (never credentials).
    #[must_use]
    pub fn requires_scopes(mut self, scopes: Vec<String>) -> Self {
        self.requires_scopes = scopes;
        self
    }
}

/// What a source can execute **natively** (RFD §6) — the planner uses this to decide
/// what to push down vs. run locally. Here a driver only *declares* its ability;
/// pushdown *planning/collapse* is E2/E3 runtime. Owned data only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PushdownProfile {
    /// Pushes nothing down; the engine does all filtering/projection locally.
    None,
    /// Pushes a declared subset down natively.
    Partial {
        /// Can push down `WHERE` predicates.
        where_: bool,
        /// Can push down projection (`SELECT` column subset).
        project: bool,
        /// Can push down `LIMIT`.
        limit: bool,
        /// Can push down `ORDER BY`.
        order: bool,
        /// Can push down a join.
        join: bool,
        /// Can push down aggregation (`COUNT`/`SUM`/… — SQL, D1, GA4 metrics).
        aggregate: bool,
        /// Can push down `DISTINCT` deduplication.
        distinct: bool,
        /// Can push down `GROUP BY` bucketing (GA4 dimensions, SQL grouping).
        group_by: bool,
    },
    /// Pushes everything down (a full SQL backend).
    Full,
}

impl PushdownProfile {
    /// Whether this profile can push `WHERE` predicates down natively. `Full` pushes
    /// everything; `None` pushes nothing; `Partial` answers from its declared flag — so
    /// the planner (t14) queries by intent instead of exhaustively destructuring.
    #[must_use]
    pub const fn supports_where(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { where_, .. } => *where_,
        }
    }

    /// Whether this profile can push projection (column subset) down natively.
    #[must_use]
    pub const fn supports_project(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { project, .. } => *project,
        }
    }

    /// Whether this profile can push `LIMIT` down natively.
    #[must_use]
    pub const fn supports_limit(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { limit, .. } => *limit,
        }
    }

    /// Whether this profile can push `ORDER BY` down natively.
    #[must_use]
    pub const fn supports_order(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { order, .. } => *order,
        }
    }

    /// Whether this profile can push a join down natively.
    #[must_use]
    pub const fn supports_join(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { join, .. } => *join,
        }
    }

    /// Whether this profile can push aggregation down natively (SQL, D1, GA4 metrics).
    #[must_use]
    pub const fn supports_aggregate(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { aggregate, .. } => *aggregate,
        }
    }

    /// Whether this profile can push `DISTINCT` down natively.
    #[must_use]
    pub const fn supports_distinct(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { distinct, .. } => *distinct,
        }
    }

    /// Whether this profile can push `GROUP BY` down natively (GA4 dimensions, SQL).
    #[must_use]
    pub const fn supports_group_by(&self) -> bool {
        match self {
            PushdownProfile::None => false,
            PushdownProfile::Full => true,
            PushdownProfile::Partial { group_by, .. } => *group_by,
        }
    }
}

/// Whether a node carries temporal coordinates (`@version`, RFD §4) — git refs, S3
/// `versionId`, Drive revisions — enabling optimistic concurrency for read-then-write.
/// `@version` path *parsing* is the addressing ticket; here a driver only declares
/// support for a resolved coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VersionSupport {
    /// No versioning — the latest state only.
    None,
    /// A single snapshot/ETag for optimistic concurrency, but no history walk.
    Snapshot,
    /// Full version history addressable by coordinate (git ref / s3 versionId / rev).
    Versioned,
}

/// A pure alias function shipped in a driver's prelude (RFD-0001 §3, e.g.
/// `fn SEND(d) = d |> CALL mail.send`).
///
/// Aliases are **pure functions in the registry**, never keywords; they desugar to a
/// `CALL` and are in scope only for plans whose driver provides them (receiver-typed
/// resolution). Owned data only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct AliasFn {
    /// The alias surface name, e.g. `SEND`.
    pub name: String,
    /// The qualified procedure it desugars to, e.g. `mail.send`.
    pub desugars_to: String,
}

impl AliasFn {
    /// Construct an alias function. Provided because the struct is `#[non_exhaustive]`
    /// (out-of-crate driver impls cannot use a struct literal).
    #[must_use]
    pub fn new(name: impl Into<String>, desugars_to: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            desugars_to: desugars_to.into(),
        }
    }
}

/// The consumer-side narrow driver trait (RFD-0001 §5, §9).
///
/// The introspective methods return **data** (or the typed [`Schema`]) — see the purity
/// invariant in the crate docs. The only impure seam is [`Driver::applier`], whose
/// [`PlanApplier`] the runtime invokes solely under `COMMIT`; constructing a `Plan` and
/// previewing it never call it. The trait is object-safe (`Arc<dyn Driver>`) so the
/// registries (G2) can hold trait objects.
pub trait Driver: Send + Sync {
    /// The mount point this driver answers for, e.g. `/mail`, `/s3`.
    fn mount(&self) -> &str;

    /// The driver's **plan identity** — the [`DriverId`] that lands in every
    /// [`Target`](qfs_plan::Target) routed here (RFD §9). Registry identity ([`mount`])
    /// and plan identity must not drift, so the default **derives** the id from the
    /// mount by stripping a single leading `/` (`/git` → `git`, `/` → ``). A driver
    /// whose plan id legitimately differs from its mount label (rare) overrides this;
    /// otherwise the default keeps the two in lockstep by construction.
    ///
    /// [`mount`]: Driver::mount
    fn id(&self) -> DriverId {
        DriverId::new(self.mount().strip_prefix('/').unwrap_or(self.mount()))
    }

    /// Describe a node's archetype + typed schema (powers `DESCRIBE`). Pure: returns
    /// data, no I/O. Per-node — a multi-archetype driver returns different archetypes on
    /// different sub-paths.
    ///
    /// # Errors
    /// Returns [`CfsError`] if the path does not resolve to a describable node.
    fn describe(&self, path: &Path) -> Result<NodeDesc, CfsError>;

    /// The capability set for a node — used to gate verbs at parse time (§5). Pure.
    /// Path-keyed: a driver narrows the archetype's default verbs per sub-path.
    fn capabilities(&self, path: &Path) -> Capabilities;

    /// The `CALL` targets this driver declares (RFD §3). Pure: returns owned data.
    fn procedures(&self) -> &[ProcSig];

    /// What this driver can run natively (RFD §6) — the planner's pushdown input. Pure.
    fn pushdown(&self) -> &PushdownProfile;

    /// Optional pure alias functions shipped with the driver (e.g. `SEND`). Pure.
    fn prelude(&self) -> &[AliasFn] {
        &[]
    }

    /// `@version` support for a node (RFD §4). Pure; per-node (a driver may version some
    /// sub-paths and not others). Defaults to no versioning.
    fn version_support(&self, _path: &Path) -> VersionSupport {
        VersionSupport::None
    }

    /// The **only** impure seam: the [`PlanApplier`] (t09) the runtime uses under
    /// `COMMIT` to apply this driver's effect nodes to the World. Returning the applier is
    /// pure; *invoking* it is the side-effecting op, and only the runtime does that. Real
    /// drivers (E4) inject auth here at construction, never on the contract surface.
    fn applier(&self) -> &dyn PlanApplier;
}

/// Look up a procedure a driver declares by its unqualified name — the resolve-time
/// gate for `CALL`. An undeclared `CALL` is rejected structurally (RFD §3: `CALL`
/// resolves only declared procs).
///
/// # Errors
/// [`CfsError::UnknownProcedure`] if no procedure with `name` is declared.
pub fn resolve_proc<'d>(driver: &'d dyn Driver, name: &str) -> Result<&'d ProcSig, CfsError> {
    driver
        .procedures()
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| CfsError::UnknownProcedure(name.to_string()))
}

/// The **parse/resolve-time capability gate** (RFD §5): check that `verb` is supported
/// at `path` by `driver`. Must run during resolution so an unsupported verb fails
/// *before* a `Plan` exists.
///
/// # Errors
/// [`CfsError::UnsupportedVerb`] — structured (path, verb, supported set) for AI
/// consumption — if the node does not declare `verb`.
pub fn check_capability(driver: &dyn Driver, path: &Path, verb: Verb) -> Result<(), CfsError> {
    let caps = driver.capabilities(path);
    if caps.allows(verb) {
        Ok(())
    } else {
        Err(CfsError::UnsupportedVerb {
            path: path.as_str().to_string(),
            verb: verb.label(),
            supported: caps.supported_labels(),
        })
    }
}

/// Reserved seam (do not call at E0): the only impure operation in qfs is the
/// interpreter that applies a [`Plan`] to the world (`COMMIT : Plan -> World`,
/// RFD §3 purity invariant). It is **deliberately not an introspective `Driver`
/// method** — it is reached only through [`Driver::applier`] under `COMMIT`. This
/// zero-sized marker keeps `qfs_plan::Plan` referenced from the contract crate and
/// documents that the runtime owns the apply loop.
#[doc(hidden)]
pub const fn _commit_seam_reserved_for_e2(_plan: &Plan) {
    // TODO(E2): the effect-plan interpreter applies a Plan to the world via the
    // applier(). It belongs to the runtime, never to the introspective Driver methods.
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_plan::{
        AppliedEffect, ApplyError, DriverId, EffectNode, NodeId, Plan, PlanApplier, Target, VfsPath,
    };
    use qfs_types::{Column, ColumnType, Schema};

    /// A no-I/O applier the fixture driver hands back. It records nothing and reports a
    /// fixed affected count — exercising the `applier()` seam with **no live creds**.
    #[derive(Default)]
    struct InMemoryApplier;

    impl PlanApplier for InMemoryApplier {
        fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
            // Uses the additive `AppliedEffect::new` (t13): an out-of-crate-style applier
            // building a success value without the (non_exhaustive) struct literal.
            Ok(AppliedEffect::new(node.id, 0))
        }
    }

    /// An in-memory multi-archetype fixture driver (no live creds, no I/O):
    /// - `/fix/blob/*`  → blob namespace (ls/cp/mv/rm), no writes.
    /// - `/fix/rel`     → relational table (select/insert/upsert/update), versioned.
    /// - `/fix/log`     → append log (select+insert only — no UPDATE, the gate target).
    struct FixtureDriver {
        procs: Vec<ProcSig>,
        pushdown: PushdownProfile,
        applier: InMemoryApplier,
        prelude: Vec<AliasFn>,
    }

    impl FixtureDriver {
        fn new() -> Self {
            Self {
                procs: vec![
                    // A declared, irreversible proc (the CALL/irreversible target).
                    ProcSig::new("send")
                        .with_params(vec![Param::new("to", ColumnType::Text)])
                        .irreversible(true)
                        .requires_scopes(vec!["mail.send".to_string()]),
                    // A reversible, row-returning proc.
                    ProcSig::new("search").returns(Schema::new(vec![Column::new(
                        "id",
                        ColumnType::Text,
                        false,
                    )])),
                ],
                pushdown: PushdownProfile::Partial {
                    where_: true,
                    project: true,
                    limit: true,
                    order: false,
                    join: false,
                    aggregate: false,
                    distinct: false,
                    group_by: false,
                },
                applier: InMemoryApplier,
                prelude: vec![AliasFn::new("SEND", "fix.send")],
            }
        }

        fn is_log(path: &Path) -> bool {
            path.as_str().starts_with("/fix/log")
        }

        fn is_rel(path: &Path) -> bool {
            path.as_str().starts_with("/fix/rel")
        }
    }

    impl Driver for FixtureDriver {
        fn mount(&self) -> &str {
            "/fix"
        }

        fn describe(&self, path: &Path) -> Result<NodeDesc, CfsError> {
            // Pure: builds data in memory; touches no filesystem, network, or clock.
            if Self::is_log(path) {
                Ok(NodeDesc::new(
                    Archetype::AppendLog,
                    Schema::new(vec![
                        Column::new("ts", ColumnType::Timestamp, false),
                        Column::new("body", ColumnType::Text, false),
                    ]),
                ))
            } else if Self::is_rel(path) {
                Ok(NodeDesc::new(
                    Archetype::RelationalTable,
                    Schema::new(vec![
                        Column::new("id", ColumnType::Int, false),
                        Column::new("name", ColumnType::Text, true),
                    ]),
                ))
            } else {
                Ok(NodeDesc::new(
                    Archetype::BlobNamespace,
                    Schema::new(vec![Column::new("name", ColumnType::Text, false)]),
                ))
            }
        }

        fn capabilities(&self, path: &Path) -> Capabilities {
            // Per-node: the append log allows select+insert but NOT update/remove.
            if Self::is_log(path) {
                Capabilities {
                    select: true,
                    insert: true,
                    ..Capabilities::default()
                }
            } else if Self::is_rel(path) {
                Capabilities {
                    select: true,
                    insert: true,
                    upsert: true,
                    update: true,
                    ..Capabilities::default()
                }
            } else {
                Capabilities {
                    ls: true,
                    cp: true,
                    mv: true,
                    rm: true,
                    ..Capabilities::default()
                }
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

        fn version_support(&self, path: &Path) -> VersionSupport {
            // Per-node: the relational table is versioned, the rest are not.
            if Self::is_rel(path) {
                VersionSupport::Versioned
            } else {
                VersionSupport::None
            }
        }

        fn applier(&self) -> &dyn PlanApplier {
            &self.applier
        }
    }

    /// G3 — the purity proof for the introspective half. None of these methods can do
    /// I/O (no `&mut self`, no future, no executor in the signatures), so this no-I/O
    /// fixture compiling and round-tripping data IS the type-level proof.
    #[test]
    fn fixture_driver_introspection_is_pure() {
        let d = FixtureDriver::new();
        let blob = Path::new("/fix/blob/a.txt");
        let rel = Path::new("/fix/rel");
        let log = Path::new("/fix/log");

        assert_eq!(d.mount(), "/fix");
        assert_eq!(
            d.describe(&blob).unwrap().archetype,
            Archetype::BlobNamespace
        );
        assert_eq!(
            d.describe(&rel).unwrap().archetype,
            Archetype::RelationalTable
        );
        assert_eq!(d.describe(&log).unwrap().archetype, Archetype::AppendLog);
        assert!(d.capabilities(&blob).ls);
        assert!(matches!(d.pushdown(), PushdownProfile::Partial { .. }));
        assert_eq!(d.prelude().len(), 1);
        assert_eq!(d.version_support(&rel), VersionSupport::Versioned);
        assert_eq!(d.version_support(&blob), VersionSupport::None);
    }

    /// describe returns the canonical typed `qfs_types::Schema` — the reconciliation
    /// (NodeSchema → NodeDesc{archetype, Schema}). Typed columns, not bare names.
    #[test]
    fn describe_returns_typed_schema() {
        let d = FixtureDriver::new();
        let desc = d.describe(&Path::new("/fix/rel")).unwrap();
        assert_eq!(desc.schema.columns.len(), 2);
        assert_eq!(desc.schema.column("id").unwrap().ty, ColumnType::Int);
        assert!(desc.schema.column("name").unwrap().nullable);
    }

    /// Capability golden gate: planning `UPDATE` against the append-only node is rejected
    /// at resolve time with a structured error listing the supported verbs.
    #[test]
    fn update_on_append_log_is_rejected_structurally() {
        let d = FixtureDriver::new();
        let log = Path::new("/fix/log");
        let err = check_capability(&d, &log, Verb::Update).unwrap_err();
        match &err {
            CfsError::UnsupportedVerb {
                path,
                verb,
                supported,
            } => {
                assert_eq!(path, "/fix/log");
                assert_eq!(*verb, "UPDATE");
                assert_eq!(supported, &vec!["SELECT", "INSERT"]);
            }
            other => panic!("expected UnsupportedVerb, got {other:?}"),
        }
        assert_eq!(err.code(), "unsupported_verb");
        // Supported verbs pass the gate.
        assert!(check_capability(&d, &log, Verb::Select).is_ok());
        assert!(check_capability(&d, &log, Verb::Insert).is_ok());
    }

    /// CALL resolves only a declared proc; an undeclared CALL is rejected structurally.
    #[test]
    fn call_resolves_only_declared_procedures() {
        let d = FixtureDriver::new();
        let send = resolve_proc(&d, "send").unwrap();
        assert!(send.irreversible, "mail.send is declared irreversible");
        assert_eq!(send.params.len(), 1);
        assert_eq!(send.requires_scopes, vec!["mail.send".to_string()]);

        let undeclared = resolve_proc(&d, "nuke").unwrap_err();
        assert_eq!(undeclared.code(), "unknown_procedure");
    }

    /// Plan assertion: a `CALL fix.send(...)` produces a Plan with one irreversible Call
    /// node and NO I/O is performed (the applier is in-memory, no live creds). The plan
    /// is built purely; only commit through the applier() seam would touch the world.
    #[test]
    fn call_builds_irreversible_plan_node_without_io() {
        let d = FixtureDriver::new();
        let send = resolve_proc(&d, "send").unwrap();

        // The evaluator (E1) would build this; here we assert the contract supports it:
        // the proc is declared irreversible, so the node is tagged irreversible.
        let target = Target::new(DriverId::new("fix"), VfsPath::new("/fix/log"));
        let node = EffectNode::new(
            NodeId(0),
            qfs_plan::EffectKind::Call(qfs_plan::ProcId::new("fix.send")),
            target,
        )
        .irreversible(send.irreversible);
        assert!(node.irreversible);

        let plan = Plan::leaf(node);
        assert_eq!(
            plan.nodes().len(),
            1,
            "exactly one Call node, no I/O performed"
        );

        // The applier seam exists and is callable (in-memory; would be the only impure
        // path under COMMIT). We do NOT commit here — building the plan does no I/O.
        let _seam: &dyn PlanApplier = d.applier();
    }

    /// The driver is object-safe (`dyn Driver`) — required because the registries store
    /// `Arc<dyn Driver>` (G2). The applier seam is reachable through the trait object.
    #[test]
    fn driver_is_object_safe() {
        let d: std::sync::Arc<dyn Driver> = std::sync::Arc::new(FixtureDriver::new());
        assert_eq!(d.mount(), "/fix");
        let _seam: &dyn PlanApplier = d.applier();
    }

    /// `DESCRIBE` JSON projection is stable (snapshot): the owned DTOs serialize
    /// deterministically for AI consumption (`-json`). We serialize each DTO **directly**
    /// (struct/enum serialization preserves declaration order, unlike a `json!` Map which
    /// re-sorts keys), so the snapshot pins the real wire shape.
    #[test]
    fn describe_json_snapshot_is_stable() {
        let d = FixtureDriver::new();
        let path = Path::new("/fix/rel");

        let describe = serde_json::to_string_pretty(&d.describe(&path).unwrap()).unwrap();
        assert_eq!(
            describe,
            r#"{
  "archetype": "relational_table",
  "schema": {
    "columns": [
      {
        "name": "id",
        "ty": "Int",
        "nullable": false,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      },
      {
        "name": "name",
        "ty": "Text",
        "nullable": true,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      }
    ]
  }
}"#
        );

        let caps = serde_json::to_string_pretty(&d.capabilities(&path)).unwrap();
        assert_eq!(
            caps,
            r#"{
  "select": true,
  "insert": true,
  "upsert": true,
  "update": true,
  "remove": false,
  "ls": false,
  "cp": false,
  "mv": false,
  "rm": false
}"#
        );

        let procs = serde_json::to_string_pretty(d.procedures()).unwrap();
        assert_eq!(
            procs,
            r#"[
  {
    "name": "send",
    "params": [
      {
        "name": "to",
        "ty": "Text"
      }
    ],
    "irreversible": true,
    "returns": null,
    "requires_scopes": [
      "mail.send"
    ]
  },
  {
    "name": "search",
    "params": [],
    "irreversible": false,
    "returns": {
      "columns": [
        {
          "name": "id",
          "ty": "Text",
          "nullable": false,
          "provenance": {
            "driver": null,
            "source_col": null
          }
        }
      ]
    },
    "requires_scopes": []
  }
]"#
        );

        let pushdown = serde_json::to_string(d.pushdown()).unwrap();
        assert_eq!(
            pushdown,
            r#"{"partial":{"where_":true,"project":true,"limit":true,"order":false,"join":false,"aggregate":false,"distinct":false,"group_by":false}}"#
        );

        let version = serde_json::to_string(&d.version_support(&path)).unwrap();
        assert_eq!(version, r#""versioned""#);
    }

    /// O1 — the default `id()` derives the plan [`DriverId`] from the mount by stripping
    /// the leading `/`, so registry identity and plan identity cannot drift. The fixture
    /// mounts at `/fix`, so its id is `fix`.
    #[test]
    fn driver_id_defaults_from_mount() {
        let d = FixtureDriver::new();
        assert_eq!(d.mount(), "/fix");
        assert_eq!(d.id(), DriverId::new("fix"));
        // Reachable through the trait object the registries store (G2).
        let dynd: std::sync::Arc<dyn Driver> = std::sync::Arc::new(FixtureDriver::new());
        assert_eq!(dynd.id(), DriverId::new("fix"));
    }

    /// O2 — verb/capability tie: every [`Verb`] in the closed set has a corresponding
    /// [`Capabilities`] flag, and `Capabilities::with(v)` enables exactly that one verb
    /// (and `allows(v)` reports it). This binds the two so a future verb cannot be added
    /// without a capability flag — they cannot silently drift apart.
    #[test]
    fn every_verb_has_a_capability_flag() {
        // `allows` is total over the closed verb set: a full capability set allows all.
        let all = Capabilities::from_verbs(&Capabilities::ALL_VERBS);
        for &verb in &Capabilities::ALL_VERBS {
            assert!(all.allows(verb), "{} not allowed by full set", verb.label());
        }
        // `supported_labels` of the full set is exactly every verb label, in order.
        let labels: Vec<&str> = Capabilities::ALL_VERBS.iter().map(|v| v.label()).collect();
        assert_eq!(all.supported_labels(), labels);

        // Enabling one verb enables exactly that verb — the tie is 1:1, no aliasing.
        for &verb in &Capabilities::ALL_VERBS {
            let only = Capabilities::none().with(verb);
            for &other in &Capabilities::ALL_VERBS {
                assert_eq!(
                    only.allows(other),
                    verb == other,
                    "{} leaked into {}",
                    verb.label(),
                    other.label()
                );
            }
        }
    }

    /// O3 — a driver declares aggregate/distinct/group_by pushdown (the SQL/D1/GA4 case),
    /// and the planner queries it by intent via the `supports_*` accessors rather than
    /// destructuring the variant.
    #[test]
    fn pushdown_declares_wide_vocabulary() {
        let profile = PushdownProfile::Partial {
            where_: true,
            project: false,
            limit: false,
            order: false,
            join: false,
            aggregate: true,
            distinct: true,
            group_by: true,
        };
        assert!(profile.supports_where());
        assert!(!profile.supports_project());
        assert!(profile.supports_aggregate());
        assert!(profile.supports_distinct());
        assert!(profile.supports_group_by());
        assert!(!profile.supports_join());

        // The endpoints answer by intent too: Full pushes the new verbs, None pushes none.
        assert!(PushdownProfile::Full.supports_aggregate());
        assert!(PushdownProfile::Full.supports_group_by());
        assert!(!PushdownProfile::None.supports_aggregate());
        assert!(!PushdownProfile::None.supports_distinct());
    }
}
