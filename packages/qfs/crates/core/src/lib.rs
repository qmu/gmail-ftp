//! `qfs-core` — the shared engine glue (RFD-0001 §3, §6).
//!
//! This is the hub every face of qfs routes through. It owns the three open
//! registries ([`MountRegistry`], [`ProcRegistry`], [`CodecRegistry`]), the
//! [`Engine`] / [`Session`] execution context that threads them (plus the reserved
//! audit-sink and capability seams), and **re-exports** the trait seams and the
//! structured [`CfsError`] so the rest of the workspace sees one consistent surface.
//!
//! ## Why both faces route through here (boundary B1)
//! The CLI (`qfs run` / interactive shell) and the server (`qfs serve`) are two
//! dispatch arms of the *same* binary reaching the *same* [`Engine`]. `qfs-cmd`
//! depends on this crate only; it never reaches past it into `qfs-lang` / `qfs-plan`
//! / `qfs-driver` / `qfs-codec` directly (fidelity guard G5 / acceptance criterion
//! C4, mechanically enforced by `tests/dep_direction.rs`).
//!
//! ## Upward edge to the parser (acceptance criterion C5, wired at E1/t06)
//! The `qfs-core → qfs-parser` edge is now **wired**: name resolution ([`resolve`])
//! consumes the parsed `qfs_parser::Statement` AST. The edge is one-directional
//! (`qfs-parser` never depends on `qfs-core`), so the spine stays acyclic;
//! `tests/dep_direction.rs` asserts the edge is now present and the back-edge absent.
//!
//! ## wasm-friendliness (boundary guard B7)
//! No threads, no `std::fs`, no sockets here. All I/O is behind (future) driver impls.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod ddl;
mod describe;
mod eval;
mod plan;
mod registry;
mod resolve;
mod security;
mod stdlib;

pub use ddl::server::{
    binding_config_row, config_row_batch, desugar_to_insert, from_server_ddl, normalize_spans,
    parse_server_binding_ddl, server_node_schema, server_write_plan, ConfigRow,
    DdlError as ServerDdlError, DesugarToInsert, EndpointDecl, EventRef, HttpMethod, Interval,
    JobDecl, PlanSpec, PolicyRef, Route, ServerBindingDdl, StatementSpec, TriggerDecl, ViewDecl,
    WebhookDecl, CREATE_WRITE_OP,
};
pub use describe::{archetype_hint, DescribeReport, PushdownSummary};
pub use eval::{call_proc_id, effect_kind_for, EvalError, EvalValue, Evaluator, PlanSource};
pub use plan::{plan_pipeline, plan_query, source_registry, PushdownError};
pub use registry::{CodecRegistry, MountRegistry, ProcRegistry};
pub use resolve::{capability_verb_for, write_verb_for, ResolveError, ResolvedCall, Resolver};
pub use security::{Ack, IrreversibleGuard, NeedsPreview, RunMode};
pub use stdlib::{
    AggregateFactory, AggregateKind, AggregateState, AliasDecl, BuiltinEval, BuiltinFn, EnvSource,
    EvalCtx, FnError, FnSig, MapEnv, NoEnv, PlanNode, PlanNodeKind, Prelude, PreludeError,
    ResolvedAlias, StdlibRegistry,
};

// Re-export the trait seams and shared types so consumers depend on `qfs-core` only.
pub use qfs_codec::Codec;
// t40: the frozen language reference (RESERVED_KEYWORDS + grammar_ebnf) and the builtin codec
// set, re-exported through qfs-core so the doc generator (in the `qfs` binary) reads them via
// `qfs-core` ONLY — it must not take a direct `qfs-lang` / `qfs-codec` edge (the lower-spine
// dep-direction guard). qfs-core already depends on both, so this adds no new edge.
pub use qfs_codec::builtin_codecs;
pub use qfs_driver::{
    check_capability, resolve_proc, AliasFn, Archetype, Capabilities, CfsError, Driver, NodeDesc,
    Param, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
pub use qfs_lang::{grammar_ebnf, RESERVED_KEYWORDS};
pub use qfs_plan::{
    commit, preview, Affected, AppliedEffect, ApplyError, CommitReport, EffectKind, EffectNode,
    NodeId, Plan, PlanApplier, PlanBuilder, PlanError, Preview, PreviewRow, ProcId,
    RecordingApplier, ServerNode, ServerWriteOp, Target, VfsPath,
};
// The canonical type & schema model (t05), re-exported so consumers see one
// `qfs_core::Schema` / `Value` / `TypeError` surface.
pub use qfs_types::{
    typecheck_predicate, CmpOp, ColRef, Column, ColumnType, DriverId, Literal, Name, Pattern,
    Predicate, Provenance, Row, RowBatch, Schema, SchemaSource, TypeError, Typed, TypedPredicate,
    Value,
};
// The credential / secret store + multi-account resolution (t27, RFD §10), re-exported
// so drivers and the server reach the one secrets surface through `qfs-core`. `Secret`
// is the only type holding key material (redacting Debug/Display, no Clone/Serialize,
// zeroized on drop); the store keys by `(driver, account)` and the resolver turns a
// statement's context into a concrete account, recording the `AccountSource` for the
// audit ledger — never the credential.
pub use qfs_secrets::{
    grant_scopes, resolve as resolve_account, AccountId, AccountIdError, AccountRecord,
    AccountSource, ActiveAccounts, CredentialKey, EnvStore, InMemoryStore,
    Resolution as AccountResolution, ResolveError as AccountResolveError, ScopeError, ScopeGrant,
    Secret, SecretError, Secrets,
};

/// The output mode for a session (RFD-0001 §7: `-json` vs human).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Human-readable output (the default for the interactive shell).
    #[default]
    Human,
    /// Machine-readable JSON envelope (AI-facing, RFD §5).
    Json,
}

/// The audit sink seam (RFD-0001 §6/§10): the applied-effect ledger that records
/// every committed effect (and, on the server, every fired plan). **Reserved for
/// E2** — E0 ships only the trait so [`Engine`] has a place to thread it. The Go
/// `internal/audit` package (append-only, owned data, never credentials, never
/// breaks the op) is the proof-of-concept this generalises.
pub trait AuditSink: Send + Sync {
    /// Record an audit entry. E0: no entry types exist yet; E2 defines them.
    fn record(&self, _summary: &str);
}

/// The process-wide execution context (RFD-0001 §6).
///
/// Owns the three registries, the reserved audit-sink hook, and the capability gate
/// handle (shape only; enforcement is E5). Constructed once per CLI invocation or
/// server boot; shared by every [`Session`].
#[derive(Default)]
pub struct Engine {
    /// Path mounts → drivers.
    pub mounts: MountRegistry,
    /// Functions + `CALL` procedures.
    pub procs: ProcRegistry,
    /// Codecs.
    pub codecs: CodecRegistry,
    /// Reserved audit-sink hook (E2). `None` until a sink is installed.
    pub audit_sink: Option<std::sync::Arc<dyn AuditSink>>,
    /// The credential / secret store (t27, RFD §10): the one [`Secrets`] surface the
    /// driver-bind context fetches credentials from at COMMIT time, keyed by
    /// `(driver, account)`. `None` until a backend is installed (the CLI installs a
    /// [`qfs_secrets::LocalStore`]; the server a `WorkerStore`/`EnvStore`). Held behind
    /// the trait so the engine is oblivious to the backend, and a `Plan` never embeds a
    /// secret — only an account *selector* (purity invariant, RFD §3).
    pub secrets: Option<std::sync::Arc<dyn Secrets>>,
    /// Reserved capability-enforcement flag (E5). Shape only at E0.
    pub capabilities_enforced: bool,
}

impl Engine {
    /// A fresh engine with three empty registries, no audit sink, and no secret store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the credential / secret store (t27). The driver-bind context fetches
    /// credentials through this handle at COMMIT time; before one is installed,
    /// credential resolution is unavailable (a driver that needs a secret fails with a
    /// structured error rather than running unauthenticated).
    #[must_use]
    pub fn with_secrets(mut self, secrets: std::sync::Arc<dyn Secrets>) -> Self {
        self.secrets = Some(secrets);
        self
    }
}

/// A single interaction's state (RFD-0001 §6, §7).
///
/// The interactive shell's cwd `{driver, path}`, the output mode, and (on the
/// server) the request/event context. The [`Engine`] is shared; the `Session` is
/// per-statement / per-request.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// Output mode for this interaction.
    pub output: OutputMode,
    /// The current working path for the interactive shell (cwd `{driver, path}`),
    /// `None` for one-shot `qfs run` (which uses absolute paths, RFD §7).
    pub cwd: Option<Path>,
    /// Reserved seam (RFD §6/§10): whether the current plan is irreversible. Mirrors
    /// `qfs_plan::Plan::is_irreversible`; threaded here so the server can apply
    /// `PREVIEW` + `POLICY` gating in E7.
    pub irreversible: bool,
}

impl Session {
    /// A fresh human-output session with no cwd.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_starts_with_three_empty_registries() {
        let e = Engine::new();
        assert!(e.mounts.is_empty());
        assert!(e.procs.is_empty());
        assert!(e.codecs.is_empty());
        assert!(e.audit_sink.is_none());
        assert!(e.secrets.is_none());
        assert!(!e.capabilities_enforced);
    }

    #[test]
    fn engine_threads_a_secrets_handle_through_the_bind_context() {
        // The driver-bind context fetches credentials via the one `Secrets` trait (t27).
        // An in-memory backend (no fs, no network, no real keychain) proves the wiring.
        let store = std::sync::Arc::new(InMemoryStore::new());
        let key = CredentialKey::new(DriverId::new("mail"), AccountId::new("work").unwrap());
        store.put(&key, Secret::from("tok")).unwrap();

        let engine = Engine::new().with_secrets(store);
        let secrets = engine.secrets.as_ref().expect("secrets installed");
        assert_eq!(secrets.get(&key).unwrap().expose_str(), Some("tok"));
        // A redacted Debug never leaks the value, even through the engine handle.
        assert!(!format!("{:?}", secrets.get(&key).unwrap()).contains("tok"));
    }

    #[test]
    fn session_defaults_to_human_no_cwd() {
        let s = Session::new();
        assert_eq!(s.output, OutputMode::Human);
        assert!(s.cwd.is_none());
        assert!(!s.irreversible);
    }

    #[test]
    fn qfs_error_codes_are_stable() {
        assert_eq!(
            CfsError::NotImplemented { feature: "x" }.code(),
            "not_implemented"
        );
    }
}
