//! `cfs-core` — the shared engine glue (RFD-0001 §3, §6).
//!
//! This is the hub every face of cfs routes through. It owns the three open
//! registries ([`MountRegistry`], [`ProcRegistry`], [`CodecRegistry`]), the
//! [`Engine`] / [`Session`] execution context that threads them (plus the reserved
//! audit-sink and capability seams), and **re-exports** the trait seams and the
//! structured [`CfsError`] so the rest of the workspace sees one consistent surface.
//!
//! ## Why both faces route through here (boundary B1)
//! The CLI (`cfs run` / interactive shell) and the server (`cfs serve`) are two
//! dispatch arms of the *same* binary reaching the *same* [`Engine`]. `cfs-cmd`
//! depends on this crate only; it never reaches past it into `cfs-lang` / `cfs-plan`
//! / `cfs-driver` / `cfs-codec` directly (fidelity guard G5 / acceptance criterion
//! C4, mechanically enforced by `tests/dep_direction.rs`).
//!
//! ## Upward edge to the parser (acceptance criterion C5, wired at E1/t06)
//! The `cfs-core → cfs-parser` edge is now **wired**: name resolution ([`resolve`])
//! consumes the parsed `cfs_parser::Statement` AST. The edge is one-directional
//! (`cfs-parser` never depends on `cfs-core`), so the spine stays acyclic;
//! `tests/dep_direction.rs` asserts the edge is now present and the back-edge absent.
//!
//! ## wasm-friendliness (boundary guard B7)
//! No threads, no `std::fs`, no sockets here. All I/O is behind (future) driver impls.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod eval;
mod registry;
mod resolve;
mod stdlib;

pub use eval::{call_proc_id, effect_kind_for, EvalError, EvalValue, Evaluator, PlanSource};
pub use registry::{CodecRegistry, MountRegistry, ProcRegistry};
pub use resolve::{capability_verb_for, write_verb_for, ResolveError, ResolvedCall, Resolver};
pub use stdlib::{
    AggregateFactory, AggregateKind, AggregateState, AliasDecl, BuiltinEval, BuiltinFn, EnvSource,
    EvalCtx, FnError, FnSig, MapEnv, NoEnv, PlanNode, PlanNodeKind, Prelude, PreludeError,
    ResolvedAlias, StdlibRegistry,
};

// Re-export the trait seams and shared types so consumers depend on `cfs-core` only.
pub use cfs_codec::Codec;
pub use cfs_driver::{
    check_capability, resolve_proc, AliasFn, Archetype, Capabilities, CfsError, Driver, NodeDesc,
    Param, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
pub use cfs_plan::{
    commit, preview, Affected, AppliedEffect, ApplyError, CommitReport, EffectKind, EffectNode,
    Plan, PlanApplier, PlanBuilder, PlanError, Preview, PreviewRow, RecordingApplier, Target,
    VfsPath,
};
// The canonical type & schema model (t05), re-exported so consumers see one
// `cfs_core::Schema` / `Value` / `TypeError` surface.
pub use cfs_types::{
    typecheck_predicate, CmpOp, ColRef, Column, ColumnType, DriverId, Literal, Name, Pattern,
    Predicate, Provenance, Row, RowBatch, Schema, SchemaSource, TypeError, Typed, TypedPredicate,
    Value,
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
    /// Reserved capability-enforcement flag (E5). Shape only at E0.
    pub capabilities_enforced: bool,
}

impl Engine {
    /// A fresh engine with three empty registries and no audit sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// `None` for one-shot `cfs run` (which uses absolute paths, RFD §7).
    pub cwd: Option<Path>,
    /// Reserved seam (RFD §6/§10): whether the current plan is irreversible. Mirrors
    /// `cfs_plan::Plan::is_irreversible`; threaded here so the server can apply
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
        assert!(!e.capabilities_enforced);
    }

    #[test]
    fn session_defaults_to_human_no_cwd() {
        let s = Session::new();
        assert_eq!(s.output, OutputMode::Human);
        assert!(s.cwd.is_none());
        assert!(!s.irreversible);
    }

    #[test]
    fn cfs_error_codes_are_stable() {
        assert_eq!(
            CfsError::NotImplemented { feature: "x" }.code(),
            "not_implemented"
        );
    }
}
