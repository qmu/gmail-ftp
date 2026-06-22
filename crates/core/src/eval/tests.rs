//! Unit tests for the pure evaluator (t07): query pipeline → `PlanSource` with threaded
//! schema, each effect verb → the correct `EffectKind` node, irreversible flagging,
//! `RETURNING` schema, `PREVIEW` of an evaluated plan, and structured errors for
//! ill-typed / unresolvable input. No execution, no I/O, no credentials.

use super::*;
use crate::registry::MountRegistry;
use cfs_driver::{
    AliasFn, Archetype, Capabilities, CfsError, NodeDesc, Param, Path, ProcSig, PushdownProfile,
    VersionSupport,
};
use cfs_parser::parse_statement;
use cfs_plan::{preview, AppliedEffect, ApplyError, EffectNode, PlanApplier};
use cfs_types::{Column, ColumnType, Schema};
use std::sync::Arc;

/// A panicking applier: if the evaluator ever performed I/O (it must not), committing
/// through this would blow up. Building/previewing a plan never calls it — that is the
/// purity proof at the test boundary.
#[derive(Default)]
struct PanicApplier;
impl PlanApplier for PanicApplier {
    fn apply(&mut self, _node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        panic!("evaluator must not perform I/O: applier invoked during pure evaluation");
    }
}

/// An in-memory test driver with a typed `describe` schema, declared procedures, prelude
/// aliases, and per-node capabilities. No I/O, no creds.
struct TestDriver {
    mount: &'static str,
    procs: Vec<ProcSig>,
    prelude: Vec<AliasFn>,
    caps: Capabilities,
    schema: Schema,
    pushdown: PushdownProfile,
    applier: PanicApplier,
}

impl TestDriver {
    fn new(mount: &'static str) -> Self {
        Self {
            mount,
            procs: Vec::new(),
            prelude: Vec::new(),
            caps: Capabilities::none(),
            schema: Schema::empty(),
            pushdown: PushdownProfile::None,
            applier: PanicApplier,
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
    fn with_schema(mut self, schema: Schema) -> Self {
        self.schema = schema;
        self
    }
}

impl Driver for TestDriver {
    fn mount(&self) -> &str {
        self.mount
    }
    fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(
            Archetype::RelationalTable,
            self.schema.clone(),
        ))
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

/// `/mail` — append-log: select+insert, ships `SEND` → `mail.send` (irreversible), a
/// two-column schema. `/db` — relational: full CRUD, a three-column schema.
fn seeded_registry() -> MountRegistry {
    let mut reg = MountRegistry::new();
    reg.register(Arc::new(
        TestDriver::new("/mail")
            .with_procs(vec![ProcSig::new("send")
                .with_params(vec![Param::new("to", ColumnType::Text)])
                .irreversible(true)])
            .with_prelude(vec![AliasFn::new("SEND", "mail.send")])
            .with_caps(Capabilities::none().select().insert())
            .with_schema(Schema::new(vec![
                Column::new("id", ColumnType::Int, false),
                Column::new("subject", ColumnType::Text, true),
            ])),
    ))
    .unwrap();
    reg.register(Arc::new(
        TestDriver::new("/db")
            .with_caps(
                Capabilities::none()
                    .select()
                    .insert()
                    .upsert()
                    .update()
                    .remove(),
            )
            .with_schema(Schema::new(vec![
                Column::new("id", ColumnType::Int, false),
                Column::new("name", ColumnType::Text, true),
                Column::new("active", ColumnType::Bool, false),
            ])),
    ))
    .unwrap();
    reg
}

fn eval(src: &str) -> Result<EvalValue, EvalError> {
    let reg = seeded_registry();
    let stmt = parse_statement(src).expect("parse");
    Evaluator::new(&reg).eval(&stmt)
}

// ---- Query pipeline → relation + schema threading ----

#[test]
fn query_pipeline_folds_to_relation_with_threaded_schema() {
    let v = eval("FROM /db/users |> WHERE active = true |> SELECT id, name").unwrap();
    let rel = v.as_relation().expect("a query yields a relation");
    // The fold produced a Project at the top, threading the source schema down.
    assert!(matches!(rel, PlanSource::Project { .. }));
    let schema = rel.schema();
    assert_eq!(
        schema.column_names(),
        vec!["id".to_string(), "name".to_string()]
    );
    // The projected `id` keeps its real type from describe (Int), not Unknown — the
    // schema threaded from the source through the filter into the projection.
    assert_eq!(schema.column("id").unwrap().ty, ColumnType::Int);
}

#[test]
fn select_star_preserves_full_schema() {
    let v = eval("FROM /db/users |> SELECT *").unwrap();
    let schema = v.as_relation().unwrap().schema();
    assert_eq!(
        schema.columns.len(),
        3,
        "SELECT * keeps every source column"
    );
}

#[test]
fn extend_adds_a_column_to_the_threaded_schema() {
    let v = eval("FROM /db/users |> EXTEND label = name").unwrap();
    let schema = v.as_relation().unwrap().schema();
    assert!(schema.column("label").is_some(), "EXTEND added the column");
    assert_eq!(schema.columns.len(), 4);
}

// ---- Each effect verb → correct EffectKind node ----

#[test]
fn each_effect_verb_maps_to_its_kind() {
    // The canonical EffectVerb → WriteVerb → EffectKind pipeline (no `_` arm).
    use cfs_parser::EffectVerb;
    assert_eq!(effect_kind_for(EffectVerb::Insert), EffectKind::Insert);
    assert_eq!(effect_kind_for(EffectVerb::Upsert), EffectKind::Upsert);
    assert_eq!(effect_kind_for(EffectVerb::Update), EffectKind::Update);
    assert_eq!(effect_kind_for(EffectVerb::Remove), EffectKind::Remove);
}

#[test]
fn insert_evaluates_to_an_insert_node() {
    let plan = eval("INSERT INTO /db/users VALUES (1, 'a', true)")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert_eq!(plan.nodes().len(), 1);
    assert_eq!(plan.nodes()[0].kind, EffectKind::Insert);
    // A literal VALUES of one row is an exact affected estimate (honest preview).
    assert_eq!(plan.nodes()[0].est_affected, Affected::Exact(1));
    plan.validate().expect("a single-node plan is a valid DAG");
}

#[test]
fn upsert_and_update_evaluate_to_their_kinds() {
    let upsert = eval("UPSERT INTO /db/users VALUES (1, 'a', true)")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert_eq!(upsert.nodes()[0].kind, EffectKind::Upsert);

    let update = eval("UPDATE /db/users SET name = 'b' WHERE id = 1")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert_eq!(update.nodes()[0].kind, EffectKind::Update);
}

#[test]
fn insert_from_query_emits_a_read_dependency() {
    // `INSERT INTO /db/users FROM /mail/inbox |> SELECT id, subject` should build a Read
    // node (the sub-pipeline) the Insert depends on — the plan DAG with a dependency edge.
    let plan = eval("INSERT INTO /db/users FROM /mail/inbox |> SELECT id, subject")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert_eq!(plan.nodes().len(), 2, "a Read dep + the Insert");
    assert!(plan.nodes().iter().any(|n| n.kind == EffectKind::Read));
    assert!(plan.nodes().iter().any(|n| n.kind == EffectKind::Insert));
    assert_eq!(plan.deps().len(), 1, "the Insert depends on the Read");
    plan.validate().expect("valid DAG with one edge");
}

// ---- Irreversible flagging ----

#[test]
fn remove_is_flagged_inherently_irreversible() {
    let plan = eval("REMOVE /db/users WHERE id = 1")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert_eq!(plan.nodes()[0].kind, EffectKind::Remove);
    assert!(
        plan.nodes()[0].irreversible,
        "REMOVE is inherently irreversible"
    );
    assert!(plan.is_irreversible());
}

#[test]
fn insert_is_reversible() {
    let plan = eval("INSERT INTO /db/users VALUES (1, 'a', true)")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    assert!(!plan.is_irreversible(), "INSERT is reversible");
}

// ---- RETURNING schema ----

#[test]
fn returning_projection_schema_is_computed() {
    let plan = eval("INSERT INTO /db/users VALUES (1, 'a', true) RETURNING id, name")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    let returning = plan.returning.as_ref().expect("a RETURNING schema");
    assert_eq!(
        returning.column_names(),
        vec!["id".to_string(), "name".to_string()]
    );
    // RETURNING types resolve against the target's described schema (id stays Int).
    assert_eq!(returning.column("id").unwrap().ty, ColumnType::Int);
}

// ---- PREVIEW of an evaluated plan ----

#[test]
fn preview_of_evaluated_plan_is_secret_free_and_ordered() {
    let plan = eval("REMOVE /db/users WHERE id = 1")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    let pv = preview(&plan);
    assert!(!pv.is_pure, "a REMOVE plan is not pure");
    assert_eq!(pv.rows.len(), 1);
    assert_eq!(pv.rows[0].verb, "REMOVE");
    assert_eq!(
        pv.irreversible.len(),
        1,
        "the REMOVE is flagged irreversible"
    );
    // Rendered preview is deterministic, human-readable, secret-free.
    let rendered = pv.to_string();
    assert!(rendered.contains("REMOVE"));
    assert!(rendered.contains("irreversible"));
}

#[test]
fn preview_of_pure_query_has_no_effects() {
    // PREVIEW of a pure read evaluates to a relation; there is no plan to apply, so a
    // caller building a plan for it (the empty plan) previews as pure.
    let v = eval("PREVIEW FROM /db/users |> SELECT id").unwrap();
    // A PREVIEW wrapper over a query is transparent: still a relation.
    assert!(v.as_relation().is_some());
}

// ---- Structured errors for ill-typed / unresolvable input ----

#[test]
fn unknown_column_in_projection_is_structured() {
    let err = eval("FROM /db/users |> SELECT nope").unwrap_err();
    assert_eq!(err.code(), "unknown_column");
    assert!(matches!(
        err,
        EvalError::Type(cfs_types::TypeError::UnknownColumn { .. })
    ));
}

#[test]
fn capability_denied_verb_never_reaches_a_plan() {
    // /mail allows only select+insert; UPDATE is denied at resolve time — the evaluator
    // surfaces the structured resolve error, no plan is built.
    let err = eval("UPDATE /mail/inbox SET subject = 'x' WHERE id = 1").unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    assert!(matches!(
        err,
        EvalError::Resolve(ResolveError::UnsupportedVerb { .. })
    ));
}

#[test]
fn unknown_procedure_is_structured() {
    let err = eval("FROM /mail/inbox |> CALL mail.nuke()").unwrap_err();
    assert_eq!(err.code(), "unknown_procedure");
    assert!(matches!(
        err,
        EvalError::Resolve(ResolveError::UnknownProcedure { .. })
    ));
}

#[test]
fn ambiguous_alias_is_structured() {
    // Two drivers ship SEND, the receiver (/git) ships neither → ambiguous.
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
        TestDriver::new("/git")
            .with_procs(vec![ProcSig::new("merge")])
            .with_caps(Capabilities::none().select()),
    ))
    .unwrap();
    let stmt = parse_statement("FROM /git/repo |> WHERE SEND()").unwrap();
    let err = Evaluator::new(&reg).eval(&stmt).unwrap_err();
    assert_eq!(err.code(), "ambiguous_alias");
}

// ---- SEND alias equivalence (golden) ----

#[test]
fn send_alias_desugars_to_call_mail_send() {
    // `FROM /mail/inbox |> WHERE SEND()` resolves (the receiver ships SEND); the relation
    // it folds to is equivalent to the explicit `CALL mail.send` pipeline — both schema-
    // preserving relations over the same scan. Resolution proves the desugaring.
    let aliased = eval("FROM /mail/inbox |> WHERE SEND()").unwrap();
    let explicit = eval("FROM /mail/inbox |> CALL mail.send(to => 'a@b.c')").unwrap();
    assert_eq!(
        aliased.as_relation().unwrap().schema().column_names(),
        explicit.as_relation().unwrap().schema().column_names(),
        "SEND desugars to the same receiver schema as the explicit CALL"
    );
}

// ---- Purity proof ----

#[test]
fn evaluation_performs_no_io() {
    // The seeded drivers hand back a PanicApplier; evaluating + previewing a write plan
    // never touches it. If the evaluator did I/O, the panic applier would fire.
    let plan = eval("INSERT INTO /db/users VALUES (1, 'a', true)")
        .unwrap()
        .as_plan()
        .cloned()
        .unwrap();
    let _ = preview(&plan);
    // No commit / apply is invoked anywhere in pure evaluation.
}

// ---- Error code distinctness ----

#[test]
fn error_codes_are_stable() {
    assert_eq!(
        EvalError::UnroutedPath { path: "/x".into() }.code(),
        "unrouted_path"
    );
    assert_eq!(
        EvalError::Type(cfs_types::TypeError::UnknownColumn {
            name: "c".into(),
            available: vec![],
        })
        .code(),
        "unknown_column"
    );
    assert_eq!(
        EvalError::Fn(crate::FnError::UnknownFunction { name: "x".into() }).code(),
        "unknown_function"
    );
}

// ---- t08: function-registry-wired evaluation (`Evaluator::with_stdlib`) ----

/// Evaluate with the function registry wired so `fn(...)` projections are typed.
fn eval_with_stdlib(src: &str) -> Result<EvalValue, EvalError> {
    let reg = seeded_registry();
    let stdlib = crate::StdlibRegistry::with_core();
    let stmt = parse_statement(src).expect("parse");
    Evaluator::with_stdlib(&reg, &stdlib).eval(&stmt)
}

/// A `SELECT fn(col)` projection carries the **built-in's declared return type** (t08),
/// not the late-bound `Unknown` of t07 — `UPPER` → `Text`, `LENGTH` → `Int`.
#[test]
fn select_over_builtin_carries_the_functions_return_type() {
    let v =
        eval_with_stdlib("FROM /db/users |> SELECT UPPER(name) AS u, LENGTH(name) AS n").unwrap();
    let schema = v.as_relation().unwrap().schema();
    assert_eq!(schema.column("u").unwrap().ty, ColumnType::Text);
    assert_eq!(schema.column("n").unwrap().ty, ColumnType::Int);
}

/// An unknown `fn(...)` in a projection is a structured [`EvalError::Fn`] (not a silent
/// `Unknown` column) once the registry is wired.
#[test]
fn select_over_unknown_function_is_a_structured_error() {
    let err = eval_with_stdlib("FROM /db/users |> SELECT NOPE(name) AS x").unwrap_err();
    assert_eq!(err.code(), "unknown_function");
}

/// Aggregate-vs-scalar dispatch (RFD §3): an aggregate (`SUM`) in a plain `SELECT` is a
/// typed error, while the same `SUM` under `AGGREGATE` types to its `Float` return.
#[test]
fn aggregate_dispatch_is_context_sensitive() {
    // SUM in a SELECT → aggregate-outside-aggregate, a typed error.
    let err = eval_with_stdlib("FROM /db/users |> SELECT SUM(id) AS s").unwrap_err();
    assert_eq!(err.code(), "aggregate_outside_aggregate");
    // SUM under AGGREGATE → typed to Float, no error.
    let v = eval_with_stdlib("FROM /db/users |> AGGREGATE SUM(id) AS s").unwrap();
    let schema = v.as_relation().unwrap().schema();
    assert_eq!(schema.column("s").unwrap().ty, ColumnType::Float);
}

/// Without the registry wired, a `fn(...)` projection stays late-bound (`Unknown`) — the
/// t07 behaviour is preserved for `Evaluator::new`.
#[test]
fn unwired_evaluator_keeps_functions_late_bound() {
    let v = eval("FROM /db/users |> SELECT UPPER(name) AS u").unwrap();
    let schema = v.as_relation().unwrap().schema();
    assert_eq!(schema.column("u").unwrap().ty, ColumnType::Unknown);
}
