//! Unit tests for name resolution (t06): CALL resolution (valid + unknown
//! driver/proc/arity/arg), receiver-typed alias resolution (valid + ambiguous + not
//! provided + unknown receiver), capability gating, and the structured error surface —
//! all as resolved-AST assertions (no execution, no I/O).

use super::*;
use crate::registry::MountRegistry;
use qfs_driver::{
    AliasFn, Archetype, Capabilities, NodeDesc, Param, Path, ProcSig, PushdownProfile,
    VersionSupport,
};
use qfs_parser::parse_statement;
use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_types::{ColumnType, Schema};
use std::sync::Arc;

#[derive(Default)]
struct NoopApplier;
impl PlanApplier for NoopApplier {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        Ok(AppliedEffect::new(node.id, 0))
    }
}

/// A configurable in-memory test driver (no I/O, no creds). Each instance declares its
/// mount, its procedures, its prelude aliases, and the capabilities of its single node.
struct TestDriver {
    mount: &'static str,
    procs: Vec<ProcSig>,
    prelude: Vec<AliasFn>,
    caps: Capabilities,
    pushdown: PushdownProfile,
    applier: NoopApplier,
}

impl TestDriver {
    fn new(mount: &'static str) -> Self {
        Self {
            mount,
            procs: Vec::new(),
            prelude: Vec::new(),
            caps: Capabilities::none(),
            pushdown: PushdownProfile::None,
            applier: NoopApplier,
        }
    }
    fn with_procs(mut self, procs: Vec<ProcSig>) -> Self {
        self.procs = procs;
        self
    }
    fn with_prelude(mut self, prelude: Vec<AliasFn>) -> Self {
        self.prelude = prelude;
        self
    }
    fn with_caps(mut self, caps: Capabilities) -> Self {
        self.caps = caps;
        self
    }
}

impl Driver for TestDriver {
    fn mount(&self) -> &str {
        self.mount
    }
    fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::AppendLog, Schema::empty()))
    }
    fn capabilities(&self, _p: &Path) -> Capabilities {
        self.caps
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
    fn version_support(&self, _p: &Path) -> VersionSupport {
        VersionSupport::None
    }
    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// A registry seeded per the ticket: `mail.send` (irreversible), `git.merge` and
/// `github.merge` (distinct namespaces), prelude aliases `SEND`(mail) and `MERGE`(git).
fn seeded_registry() -> MountRegistry {
    let mut reg = MountRegistry::new();
    reg.register(Arc::new(
        TestDriver::new("/mail")
            .with_procs(vec![ProcSig::new("send")
                .with_params(vec![Param::new("to", ColumnType::Text)])
                .irreversible(true)])
            .with_prelude(vec![AliasFn::new("SEND", "mail.send")])
            .with_caps(Capabilities::none().select().insert()),
    ))
    .unwrap();
    reg.register(Arc::new(
        TestDriver::new("/git")
            .with_procs(vec![
                ProcSig::new("merge").with_params(vec![Param::new("method", ColumnType::Text)])
            ])
            .with_prelude(vec![AliasFn::new("MERGE", "git.merge")])
            .with_caps(Capabilities::none().select()),
    ))
    .unwrap();
    reg.register(Arc::new(TestDriver::new("/github").with_procs(vec![
        ProcSig::new("merge").with_params(vec![Param::new("method", ColumnType::Text)]),
    ])))
    .unwrap();
    reg
}

fn resolve(src: &str) -> Result<Vec<ResolvedCall>, ResolveError> {
    let reg = seeded_registry();
    let stmt = parse_statement(src).expect("parse");
    Resolver::new(&reg).resolve_statement(&stmt)
}

// ---- CALL resolution ----

#[test]
fn call_resolves_declared_procedure() {
    let calls = resolve("/mail/inbox |> CALL mail.send(to => 'a@b.c')").unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].qualified, "mail.send");
    assert_eq!(calls[0].driver, DriverId::new("mail"));
    assert!(calls[0].irreversible, "mail.send is declared irreversible");
}

/// Namespace isolation: `git.merge` and `github.merge` resolve to DISTINCT refs.
#[test]
fn call_namespaces_are_isolated() {
    let git = resolve("/git/repo |> CALL git.merge(method => 'squash')").unwrap();
    let github = resolve("/github/repo |> CALL github.merge(method => 'squash')").unwrap();
    assert_eq!(git[0].qualified, "git.merge");
    assert_eq!(github[0].qualified, "github.merge");
    assert_ne!(git[0].driver, github[0].driver);
    assert_ne!(git[0].qualified, github[0].qualified);
}

#[test]
fn call_unknown_driver_is_structured() {
    let err = resolve("/mail/inbox |> CALL drive.merge()").unwrap_err();
    assert_eq!(err.code(), "unknown_driver");
    assert!(matches!(err, ResolveError::UnknownDriver { driver } if driver == "drive"));
}

#[test]
fn call_unknown_procedure_lists_available() {
    let err = resolve("/mail/inbox |> CALL mail.nuke()").unwrap_err();
    assert_eq!(err.code(), "unknown_procedure");
    match err {
        ResolveError::UnknownProcedure {
            driver,
            name,
            available,
        } => {
            assert_eq!(driver, "mail");
            assert_eq!(name, "nuke");
            assert_eq!(available, vec!["send".to_string()]);
        }
        other => panic!("expected UnknownProcedure, got {other:?}"),
    }
}

#[test]
fn call_arity_mismatch_is_structured() {
    // mail.send declares ONE param; supplying two positional args overflows.
    let err = resolve("/mail/inbox |> CALL mail.send('a', 'b')").unwrap_err();
    assert_eq!(err.code(), "arity_mismatch");
    match err {
        ResolveError::ArityMismatch {
            qualified,
            expected,
            found,
        } => {
            assert_eq!(qualified, "mail.send");
            assert_eq!(expected, 1);
            assert_eq!(found, 2);
        }
        other => panic!("expected ArityMismatch, got {other:?}"),
    }
}

#[test]
fn call_unknown_named_arg_lists_params() {
    let err = resolve("/mail/inbox |> CALL mail.send(bcc => 'x')").unwrap_err();
    assert_eq!(err.code(), "unknown_arg");
    match err {
        ResolveError::UnknownArg {
            qualified,
            arg,
            params,
        } => {
            assert_eq!(qualified, "mail.send");
            assert_eq!(arg, "bcc");
            assert_eq!(params, vec!["to".to_string()]);
        }
        other => panic!("expected UnknownArg, got {other:?}"),
    }
}

// ---- Receiver-typed alias resolution ----

/// `mail-inbox |> SEND` resolves and desugars to `… CALL mail.send` (the receiver is
/// the /mail driver, which ships SEND).
#[test]
fn alias_resolves_against_receiver_and_desugars() {
    let calls = resolve("/mail/inbox |> WHERE SEND()").unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].qualified, "mail.send",
        "SEND desugars to the receiver's mail.send"
    );
    assert!(calls[0].irreversible);
}

/// An alias the receiver does NOT ship, but exactly one other driver does → not
/// provided by the receiver (NOT ambiguous: only one provider).
#[test]
fn alias_not_provided_by_receiver() {
    // MERGE is shipped only by /git; the receiver here is /mail.
    let err = resolve("/mail/inbox |> WHERE MERGE()").unwrap_err();
    assert_eq!(err.code(), "alias_not_provided");
    assert!(
        matches!(err, ResolveError::AliasNotProvided { name, driver }
        if name == "MERGE" && driver == "mail")
    );
}

/// An alias shipped by two non-receiver drivers → AmbiguousAlias naming both candidates,
/// directing the user to the qualified CALL.
#[test]
fn alias_ambiguous_across_drivers() {
    // Build a registry where SEND is shipped by BOTH /mail and /sms, and the receiver
    // (/git) ships neither — so resolution is ambiguous.
    let mut reg = MountRegistry::new();
    reg.register(Arc::new(
        TestDriver::new("/mail")
            .with_procs(vec![ProcSig::new("send")])
            .with_prelude(vec![AliasFn::new("SEND", "mail.send")]),
    ))
    .unwrap();
    reg.register(Arc::new(
        TestDriver::new("/sms")
            .with_procs(vec![ProcSig::new("send")])
            .with_prelude(vec![AliasFn::new("SEND", "sms.send")]),
    ))
    .unwrap();
    reg.register(Arc::new(
        TestDriver::new("/git").with_procs(vec![ProcSig::new("merge")]),
    ))
    .unwrap();

    let stmt = parse_statement("/git/repo |> WHERE SEND()").unwrap();
    let err = Resolver::new(&reg).resolve_statement(&stmt).unwrap_err();
    assert_eq!(err.code(), "ambiguous_alias");
    match err {
        ResolveError::AmbiguousAlias { name, candidates } => {
            assert_eq!(name, "SEND");
            assert_eq!(candidates, vec!["mail".to_string(), "sms".to_string()]);
        }
        other => panic!("expected AmbiguousAlias, got {other:?}"),
    }
}

/// An alias used over a `VALUES` source (no receiver driver) fails closed with
/// UnknownReceiver rather than guessing.
#[test]
fn alias_over_valueless_receiver_fails_closed() {
    let err = resolve("VALUES (1) |> WHERE SEND()").unwrap_err();
    assert_eq!(err.code(), "unknown_receiver");
    assert!(matches!(err, ResolveError::UnknownReceiver { name } if name == "SEND"));
}

/// A non-alias `fn(...)` call is NOT our concern (left for the function-registry
/// ticket): resolution simply produces no binding for it and does not error.
#[test]
fn non_alias_function_is_ignored() {
    let calls = resolve("/mail/inbox |> WHERE upper(subject) == 'X'").unwrap();
    assert!(
        calls.is_empty(),
        "a non-prelude function is left for the function-registry ticket"
    );
}

// ---- Capability gating ----

/// An UPDATE against the /mail node (which declares only select+insert) is rejected at
/// resolve time with a structured UnsupportedVerb carrying the supported set.
#[test]
fn effect_verb_capability_gate_rejects_unsupported() {
    let err = resolve("UPDATE /mail/inbox SET read = true").unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    match err {
        ResolveError::UnsupportedVerb {
            path,
            verb,
            supported,
        } => {
            assert_eq!(verb, "UPDATE");
            assert!(path.starts_with("/mail"));
            assert_eq!(supported, vec!["SELECT", "INSERT"]);
        }
        other => panic!("expected UnsupportedVerb, got {other:?}"),
    }
}

/// A supported verb (INSERT against /mail, which allows insert) passes the gate.
#[test]
fn effect_verb_capability_gate_allows_supported() {
    let calls = resolve("INSERT INTO /mail/inbox VALUES ('hi')").unwrap();
    assert!(calls.is_empty(), "no callables; the verb passed the gate");
}

// ---- Canonical verb mapping (t09 O2 carry-over) ----

/// `write_verb_for` is the canonical exhaustive EffectVerb -> WriteVerb map, and
/// `capability_verb_for` the EffectVerb -> Verb map; both bind the vocabularies so a new
/// verb cannot drift (the no-`_`-arm matches force this at compile time).
#[test]
fn canonical_verb_maps_are_total() {
    use qfs_parser::EffectVerb;
    assert_eq!(write_verb_for(EffectVerb::Insert), WriteVerb::Insert);
    assert_eq!(write_verb_for(EffectVerb::Upsert), WriteVerb::Upsert);
    assert_eq!(write_verb_for(EffectVerb::Update), WriteVerb::Update);
    assert_eq!(write_verb_for(EffectVerb::Remove), WriteVerb::Remove);

    assert_eq!(capability_verb_for(EffectVerb::Insert), Verb::Insert);
    assert_eq!(capability_verb_for(EffectVerb::Upsert), Verb::Upsert);
    assert_eq!(capability_verb_for(EffectVerb::Update), Verb::Update);
    assert_eq!(capability_verb_for(EffectVerb::Remove), Verb::Remove);
}

// ---- Structured error surface ----

/// Every error arm has a distinct, stable, machine-readable code (RFD §5).
#[test]
fn error_codes_are_distinct_and_stable() {
    let codes = [
        ResolveError::UnknownDriver { driver: "d".into() }.code(),
        ResolveError::UnknownProcedure {
            driver: "d".into(),
            name: "p".into(),
            available: vec![],
        }
        .code(),
        ResolveError::ArityMismatch {
            qualified: "d.p".into(),
            expected: 0,
            found: 1,
        }
        .code(),
        ResolveError::UnknownArg {
            qualified: "d.p".into(),
            arg: "a".into(),
            params: vec![],
        }
        .code(),
        ResolveError::AliasNotProvided {
            name: "X".into(),
            driver: "d".into(),
        }
        .code(),
        ResolveError::AmbiguousAlias {
            name: "X".into(),
            candidates: vec![],
        }
        .code(),
        ResolveError::UnknownReceiver { name: "X".into() }.code(),
        ResolveError::UnknownBinding { name: "X".into() }.code(),
        ResolveError::UnsupportedVerb {
            path: "/x".into(),
            verb: "UPDATE",
            supported: vec![],
        }
        .code(),
    ];
    let unique: std::collections::BTreeSet<&str> = codes.iter().copied().collect();
    assert_eq!(unique.len(), codes.len(), "every arm has a distinct code");
}

/// PREVIEW/COMMIT wrappers are transparent to resolution (the inner statement resolves).
#[test]
fn plan_wrapper_resolves_inner() {
    let calls = resolve("PREVIEW /mail/inbox |> CALL mail.send(to => 'a@b.c')").unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].qualified, "mail.send");
}

// ---- LET binding resolution (M6, ticket t60) ------------------------------

/// A `LET`-bound name resolves as a source in scope for the body — no unbound error, and
/// the body's `CALL` (against a qualified driver namespace) still resolves.
#[test]
fn let_bound_name_resolves_in_body() {
    let calls = resolve(
        "LET inbox = /mail/inbox\n\
         inbox |> CALL mail.send(to => 'a@b.c')",
    )
    .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].qualified, "mail.send");
}

/// A bare-identifier source with no matching binding is a structured `UnknownBinding`
/// error — a typo'd name is never a silent empty relation (RFD §5).
#[test]
fn unbound_name_is_structured_error() {
    let err = resolve("ghost |> LIMIT 1").unwrap_err();
    assert_eq!(err.code(), "unknown_binding");
    assert!(matches!(err, ResolveError::UnknownBinding { name } if name == "ghost"));
}

/// Shadowing is allowed: an inner `LET` of the same name wins for the statements after it.
#[test]
fn shadowing_is_allowed() {
    resolve(
        "LET x = /mail/inbox\n\
         LET x = /git/repo\n\
         x |> LIMIT 1",
    )
    .expect("a re-bound name resolves");
}

// ---- Reserved-name resolution (decision P / t71) --------------------------

/// A reserved realm name used as a bare source resolves to its fixed realm — it is a
/// legal source with no `LET` binding, never the `UnknownBinding` a non-realm bare name is.
#[test]
fn reserved_realm_name_is_a_legal_bare_source() {
    // `sys` is a reserved realm; it needs no `LET` binding to be a legal source.
    resolve("sys |> LIMIT 1").expect("a reserved realm name resolves without a binding");
    // A non-realm bare name with no binding is still the structured `UnknownBinding`.
    let err = resolve("ghost |> LIMIT 1").unwrap_err();
    assert_eq!(err.code(), "unknown_binding");
}

/// A reserved realm name is **not shadowed** by a `LET` binding of the same spelling: even
/// with `LET sys = …` in scope, `sys` resolves as the fixed realm (the realm ranks above
/// the lexical realm), so the reference is legal either way — never an unbound error.
#[test]
fn reserved_realm_name_is_not_shadowed_by_a_let_binding() {
    resolve(
        "LET sys = /mail/inbox\n\
         sys |> LIMIT 1",
    )
    .expect("a reserved realm name resolves regardless of a same-named binding");
}

/// A binding's value resolves in the OUTER scope — there is no forward/recursive reference,
/// so a value that names a not-yet-bound sibling is a structured `UnknownBinding` error.
#[test]
fn binding_value_has_no_forward_reference() {
    let err = resolve(
        "LET a = b |> LIMIT 1\n\
         LET b = /mail/inbox\n\
         a",
    )
    .unwrap_err();
    assert!(matches!(err, ResolveError::UnknownBinding { name } if name == "b"));
}
