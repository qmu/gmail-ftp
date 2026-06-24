//! The effect node: [`EffectKind`] (the closed-core write verbs) and [`EffectNode`]
//! (one effect, fully described as data). RFD-0001 §3 (closed core) / §6 (runtime).

use qfs_types::RowBatch;
use serde::Serialize;

use crate::ids::{Affected, NodeId, ProcId, Target};
use crate::server::{ServerNode, ServerWriteOp};

/// The kind of an effect — a **closed set** mirroring the frozen core write verbs
/// (RFD §3). A new backend adds **zero** variants here; it routes through a `Target`
/// driver id and (for [`EffectKind::Call`]) a [`ProcId`] string instead.
///
/// `Read`/`List` are pure data-acquisition nodes the evaluator may emit as plan
/// dependencies of a write (e.g. an `UPDATE … FROM <query>`); they are reversible.
/// `Insert`/`Upsert`/`Update`/`Remove`/`Call` are the write verbs. `Upsert` is modelled
/// **distinctly** from `Insert` so retry-safe (idempotent) effects are first-class
/// (RFD §6 recovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectKind {
    /// Read rows from the target (a pure dependency of a downstream write).
    Read,
    /// List entries at the target path (a pure dependency of a downstream write).
    List,
    /// `INSERT INTO <path>` — create rows / objects.
    Insert,
    /// `UPSERT INTO <path>` — idempotent create-or-update (retry-safe, §6).
    Upsert,
    /// `UPDATE <path>` — modify existing rows / objects.
    Update,
    /// `REMOVE <path>` — delete rows / objects (irreversible, §10).
    Remove,
    /// `CALL <driver>.<action>(...)` — an irreducible namespaced procedure (§3). The
    /// [`ProcId`] is the registry name; irreversibility is carried per-node.
    Call(ProcId),
    /// A write to the `/server/...` self-config registry (RFD §6/§8): *which* config
    /// node ([`ServerNode`]) and *which* write op ([`ServerWriteOp`]). The config payload
    /// (the owned DTO) rides in the node's [`RowBatch`] `args` — `qfs-plan` stays free of
    /// server-shaped DTOs (purity invariant), and the COMMIT-time apply that mutates
    /// `ServerState` under its `RwLock` lives in `qfs-server`. `irreversible = false`
    /// (config writes are reversible; a `Remove` is undone by re-inserting), and the op is
    /// idempotent under [`ServerWriteOp::Upsert`] so boot/replay converge (RFD §6).
    ServerConfigWrite {
        /// The `/server/...` config collection being written.
        node: ServerNode,
        /// The write op applied to it.
        op: ServerWriteOp,
    },
}

impl EffectKind {
    /// Whether this kind is *inherently* irreversible regardless of the procedure
    /// declaration. `Remove` always destroys data (RFD §10). `Call` irreversibility is
    /// declared per-procedure by the planner (e.g. `mail.send`) and recorded on the
    /// node, so it is **not** decided here.
    #[must_use]
    pub fn is_inherently_irreversible(&self) -> bool {
        matches!(self, EffectKind::Remove)
    }

    /// A short, stable label for previews / golden snapshots.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            EffectKind::Read => "READ",
            EffectKind::List => "LIST",
            EffectKind::Insert => "INSERT",
            EffectKind::Upsert => "UPSERT",
            EffectKind::Update => "UPDATE",
            EffectKind::Remove => "REMOVE",
            EffectKind::Call(_) => "CALL",
            // A stable per-node label so coalescing groups distinct `/server` collections
            // separately (each is a different node + op pairing).
            EffectKind::ServerConfigWrite { node, op } => match (node, op) {
                (ServerNode::Endpoints, ServerWriteOp::Insert) => "SERVER_ENDPOINTS_INSERT",
                (ServerNode::Endpoints, ServerWriteOp::Upsert) => "SERVER_ENDPOINTS_UPSERT",
                (ServerNode::Endpoints, ServerWriteOp::Update) => "SERVER_ENDPOINTS_UPDATE",
                (ServerNode::Endpoints, ServerWriteOp::Remove) => "SERVER_ENDPOINTS_REMOVE",
                (ServerNode::Triggers, ServerWriteOp::Insert) => "SERVER_TRIGGERS_INSERT",
                (ServerNode::Triggers, ServerWriteOp::Upsert) => "SERVER_TRIGGERS_UPSERT",
                (ServerNode::Triggers, ServerWriteOp::Update) => "SERVER_TRIGGERS_UPDATE",
                (ServerNode::Triggers, ServerWriteOp::Remove) => "SERVER_TRIGGERS_REMOVE",
                (ServerNode::Jobs, ServerWriteOp::Insert) => "SERVER_JOBS_INSERT",
                (ServerNode::Jobs, ServerWriteOp::Upsert) => "SERVER_JOBS_UPSERT",
                (ServerNode::Jobs, ServerWriteOp::Update) => "SERVER_JOBS_UPDATE",
                (ServerNode::Jobs, ServerWriteOp::Remove) => "SERVER_JOBS_REMOVE",
                (ServerNode::Views, ServerWriteOp::Insert) => "SERVER_VIEWS_INSERT",
                (ServerNode::Views, ServerWriteOp::Upsert) => "SERVER_VIEWS_UPSERT",
                (ServerNode::Views, ServerWriteOp::Update) => "SERVER_VIEWS_UPDATE",
                (ServerNode::Views, ServerWriteOp::Remove) => "SERVER_VIEWS_REMOVE",
                (ServerNode::Policies, ServerWriteOp::Insert) => "SERVER_POLICIES_INSERT",
                (ServerNode::Policies, ServerWriteOp::Upsert) => "SERVER_POLICIES_UPSERT",
                (ServerNode::Policies, ServerWriteOp::Update) => "SERVER_POLICIES_UPDATE",
                (ServerNode::Policies, ServerWriteOp::Remove) => "SERVER_POLICIES_REMOVE",
                (ServerNode::Webhooks, ServerWriteOp::Insert) => "SERVER_WEBHOOKS_INSERT",
                (ServerNode::Webhooks, ServerWriteOp::Upsert) => "SERVER_WEBHOOKS_UPSERT",
                (ServerNode::Webhooks, ServerWriteOp::Update) => "SERVER_WEBHOOKS_UPDATE",
                (ServerNode::Webhooks, ServerWriteOp::Remove) => "SERVER_WEBHOOKS_REMOVE",
            },
        }
    }
}

/// One fully-described effect: what it does ([`EffectKind`]), where it lands
/// ([`Target`]), the data it carries ([`RowBatch`] from `qfs-types`), whether it is
/// irreversible, and how many rows it is estimated to touch ([`Affected`]).
///
/// `#[non_exhaustive]` so representation can gain internal fields without a breaking
/// grammar change (the *grammar* stays frozen; the plan representation may evolve).
/// Carries **no secrets** — safe to render in a preview and log (RFD §10).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct EffectNode {
    /// The plan-local node identity (dependency endpoint, topo tie-breaker).
    pub id: NodeId,
    /// What this effect does.
    pub kind: EffectKind,
    /// Where the effect lands (driver + virtual path).
    pub target: Target,
    /// The rows the effect writes (empty for `Read`/`List`/`Remove`-by-filter).
    pub args: RowBatch,
    /// Whether applying this effect cannot be undone (`Remove`, declared-irreversible
    /// `Call`). Surfaced explicitly in [`preview`](crate::preview).
    pub irreversible: bool,
    /// The estimated row count this effect touches (honest: `Exact`/`AtMost`/`Unknown`).
    pub est_affected: Affected,
}

impl EffectNode {
    /// Construct a node, deriving `irreversible` for inherently-irreversible kinds
    /// (`Remove`) and leaving `Call` to the explicit builder below.
    #[must_use]
    pub fn new(id: NodeId, kind: EffectKind, target: Target) -> Self {
        let irreversible = kind.is_inherently_irreversible();
        Self {
            id,
            kind,
            target,
            args: RowBatch::default(),
            irreversible,
            est_affected: Affected::Unknown,
        }
    }

    /// Builder: attach the row batch the effect writes, refining `est_affected` to the
    /// exact row count when it was previously `Unknown` (an explicit estimate set via
    /// [`EffectNode::with_affected`] is preserved).
    #[must_use]
    pub fn with_args(mut self, args: RowBatch) -> Self {
        if matches!(self.est_affected, Affected::Unknown) {
            // A literal row batch gives an exact count; saturating cast is fine since
            // a usize row count fits u64 on every supported target.
            self.est_affected = Affected::Exact(args.rows.len() as u64);
        }
        self.args = args;
        self
    }

    /// Builder: set the affected-row estimate explicitly (honest bounds for
    /// filter-driven effects whose count is not known until apply).
    #[must_use]
    pub fn with_affected(mut self, est: Affected) -> Self {
        self.est_affected = est;
        self
    }

    /// Builder: mark this node irreversible explicitly (e.g. a declared-irreversible
    /// `Call` such as `mail.send`). Inherently-irreversible kinds are already flagged.
    #[must_use]
    pub fn irreversible(mut self, yes: bool) -> Self {
        self.irreversible = self.irreversible || yes;
        self
    }
}
