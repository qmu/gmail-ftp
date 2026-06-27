//! Golden + rejection tests for the closed-core server-binding DDL (t31).
//!
//! Covers: statement → AST → desugared `Plan` for all five frozen forms (+ `MATERIALIZED`);
//! the deferred body stored as a span-normalised `StatementSpec`/`PlanSpec` round-tripping
//! through serde unchanged; parse-time rejection (malformed body, unsupported column, unknown
//! `CREATE` subkeyword) as a structured error (no panic); `PREVIEW` "1 row → /server/<kind>";
//! and the CREATE ≡ INSERT body-bearing equivalence (the t30 gap closed).

use super::*;
use qfs_parser::parse_statement;
use qfs_plan::{preview, EffectKind, ServerNode, ServerWriteOp};

/// Parse a source string to a structured binding (panics in tests on error — these inputs
/// are valid by construction).
fn binding(src: &str) -> ServerBindingDdl {
    parse_server_binding_ddl(src).expect("valid binding DDL")
}

/// The single effect node of a one-node plan.
fn only_effect(plan: &qfs_plan::Plan) -> &qfs_plan::EffectNode {
    assert_eq!(plan.nodes.len(), 1, "binding desugars to exactly one node");
    &plan.nodes[0]
}

#[test]
fn endpoint_desugars_to_one_endpoints_insert() {
    let b = binding("CREATE ENDPOINT recent ON 'GET /recent' AS /mail |> LIMIT 10");
    assert_eq!(b.node(), ServerNode::Endpoints);
    let plan = b.desugar().expect("desugar");
    let node = only_effect(&plan);
    match &node.kind {
        EffectKind::ServerConfigWrite { node, op } => {
            assert_eq!(*node, ServerNode::Endpoints);
            assert_eq!(*op, ServerWriteOp::Upsert);
        }
        other => panic!("expected ServerConfigWrite, got {other:?}"),
    }
    assert_eq!(node.target.path.as_str(), "/server/endpoints");
}

#[test]
fn trigger_desugars_to_one_triggers_insert() {
    let b = binding("CREATE TRIGGER onnew ON inbox DO REMOVE /tmp WHERE age > 7");
    assert_eq!(b.node(), ServerNode::Triggers);
    let node_plan = b.desugar().expect("desugar");
    match &only_effect(&node_plan).kind {
        EffectKind::ServerConfigWrite { node, .. } => assert_eq!(*node, ServerNode::Triggers),
        other => panic!("expected ServerConfigWrite, got {other:?}"),
    }
}

#[test]
fn job_desugars_to_one_jobs_insert() {
    let b = binding("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    match &b {
        ServerBindingDdl::Job(d) => assert_eq!(d.every.as_str(), "1h"),
        other => panic!("expected a Job, got {other:?}"),
    }
    let plan = b.desugar().expect("desugar");
    match &only_effect(&plan).kind {
        EffectKind::ServerConfigWrite { node, .. } => assert_eq!(*node, ServerNode::Jobs),
        other => panic!("expected ServerConfigWrite, got {other:?}"),
    }
}

#[test]
fn view_and_materialized_view_set_the_flag() {
    let plain = binding("CREATE VIEW recent AS /mail |> LIMIT 5");
    let mat = binding("CREATE MATERIALIZED VIEW recent AS /mail |> LIMIT 5");
    match (&plain, &mat) {
        (ServerBindingDdl::View(p), ServerBindingDdl::View(m)) => {
            assert!(!p.materialized, "plain VIEW => materialized=false");
            assert!(m.materialized, "MATERIALIZED VIEW => materialized=true");
        }
        _ => panic!("expected two views"),
    }
    // Both desugar to /server/views with a `materialized` column carrying the flag.
    let row = binding_config_row(&mat);
    assert_eq!(row.get("materialized"), Some(&Value::Bool(true)));
    assert_eq!(plain.node(), ServerNode::Views);
    assert_eq!(mat.node(), ServerNode::Views);
}

#[test]
fn webhook_desugars_to_one_webhooks_insert() {
    // t04 carries the webhook route in the `ON` operand (the frozen grammar uses `ON`, not a
    // new `AT` keyword — see the placement note: t31 adds zero closed-core keywords).
    let b = binding("CREATE WEBHOOK gh ON '/hooks/gh'");
    assert_eq!(b.node(), ServerNode::Webhooks);
    let row = binding_config_row(&b);
    assert_eq!(
        row.get("route"),
        Some(&Value::Text("/hooks/gh".to_string()))
    );
}

#[test]
fn plan_spec_round_trips_through_serde_unchanged() {
    let b = binding("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let ServerBindingDdl::Job(d) = &b else {
        panic!("expected a Job");
    };
    let plan = d.plan.as_ref().expect("DO body present");
    // serde over the spec round-trips byte-identically.
    let json = serde_json::to_string(plan).expect("serialize");
    let back: PlanSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, plan, "PlanSpec round-trips unchanged");
    // The canonical form rehydrates without a re-parse.
    let canon = plan.canonical();
    let rehydrated = PlanSpec::from_canonical(&canon).expect("rehydrate");
    assert_eq!(rehydrated.canonical(), canon);
}

#[test]
fn trigger_where_guard_round_trips_into_the_predicate_spec() {
    // t34 (CO-t31-4): `CREATE TRIGGER … ON <event> WHERE <pred> DO <plan>` parses the optional
    // WHERE guard into `TriggerDecl.predicate` (a StatementSpec wrapping the predicate), and the
    // desugar emits it into the `predicate` config column — round-tripping byte-identically.
    let b = binding(
        "CREATE TRIGGER hot ON inbox WHERE priority > 3 DO INSERT INTO /log VALUES ('fired')",
    );
    let ServerBindingDdl::Trigger(d) = &b else {
        panic!("expected a trigger");
    };
    let pred = d.predicate.as_ref().expect("WHERE guard present");
    // The guard spec rehydrates without a re-parse and is the `Query |> WHERE` carrier shape.
    let canon = pred.canonical();
    let rehydrated = StatementSpec::from_canonical(&canon).expect("rehydrate");
    assert_eq!(rehydrated.canonical(), canon);

    // The desugar emits the guard into the `predicate` config column (matching the canonical spec).
    let row = binding_config_row(&b);
    let predicate_col = row.get("predicate").expect("predicate column emitted");
    assert_eq!(predicate_col, &qfs_types::Value::Text(canon));

    // A guard-less trigger emits NO predicate column (stays byte-identical to a pre-t34 trigger).
    let plain = binding("CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('x')");
    let plain_row = binding_config_row(&plain);
    assert!(
        plain_row.get("predicate").is_none(),
        "a guard-less trigger emits no predicate column"
    );
}

#[test]
fn statement_spec_is_span_normalised_so_parse_origin_does_not_matter() {
    // The same body parsed standalone vs from a CREATE wrapper must produce an identical
    // canonical spec (spans differ by origin; the spec zeroes them).
    let standalone = parse_statement("/mail |> LIMIT 10").expect("parse");
    let from_create = binding("CREATE VIEW v AS /mail |> LIMIT 10");
    let ServerBindingDdl::View(d) = &from_create else {
        panic!("expected a view");
    };
    assert_eq!(
        StatementSpec::from_statement(standalone).canonical(),
        d.query.as_ref().expect("AS body").canonical(),
        "span-normalised spec is independent of parse origin"
    );
}

#[test]
fn preview_reports_one_row_into_the_right_server_kind() {
    let b = binding("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let plan = b.desugar().expect("desugar");
    let pv = preview(&plan);
    assert!(
        !pv.is_pure,
        "a binding desugars to an effect, not a pure read"
    );
    assert_eq!(pv.rows.len(), 1, "exactly one previewed row");
    let row = &pv.rows[0];
    assert_eq!(row.target.path.as_str(), "/server/jobs");
    assert_eq!(
        row.affected,
        qfs_plan::Affected::Exact(1),
        "1 row → /server/jobs"
    );
    // PREVIEW performs no I/O — it only reads the plan (this whole test mutates nothing).
}

#[test]
fn unknown_create_subkeyword_is_a_structured_error_not_a_panic() {
    // POLICY is parsed by t04 but is not a t31 binding form (deferred to t34): a structured
    // rejection, not a panic.
    let err = parse_server_binding_ddl("CREATE POLICY p").expect_err("POLICY is not a t31 form");
    assert_eq!(err.code(), "UNSUPPORTED_DDL");
}

#[test]
fn malformed_body_is_rejected_at_create_time() {
    // A `DO <plan>` whose body does not parse is rejected at CREATE time (a parse error
    // surfaces through the structured DdlError, never at fire time).
    let err = parse_server_binding_ddl("CREATE JOB j EVERY '1h' DO this is not a statement")
        .expect_err("malformed body");
    matches!(err, DdlError::Parse(_))
        .then_some(())
        .expect("a parse error, structured");
}

#[test]
fn missing_required_clause_is_a_structured_error() {
    // CREATE JOB without EVERY is rejected with a structured MISSING_CLAUSE error.
    let err = parse_server_binding_ddl("CREATE JOB j DO REMOVE /tmp").expect_err("no EVERY");
    assert_eq!(err.code(), "MISSING_CLAUSE");
}

#[test]
fn unknown_column_in_config_row_is_rejected() {
    let mut row = ConfigRow::default();
    row.set_text("name", "x");
    row.set_text("not_a_column", "boom");
    let err = config_row_batch(ServerNode::Jobs, &row).expect_err("unknown column");
    assert_eq!(err.code(), "UNKNOWN_COLUMN");
}

#[test]
fn body_bearing_create_equals_its_insert_twin_via_canonical_spec() {
    // The t30 gap (CO-t30-2/3) closed: a body-bearing CREATE and its hand-written INSERT twin
    // now store the IDENTICAL canonical plan-body spec, because the INSERT's `plan` string
    // column is parsed into the same StatementSpec the CREATE form builds.
    let create = binding("CREATE JOB x EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let ServerBindingDdl::Job(d) = &create else {
        panic!("expected a job");
    };
    let create_body = d.plan.as_ref().expect("DO body").canonical();

    // The INSERT twin supplies the body as a source STRING; the /server typing parses it into
    // the same canonical spec.
    let insert_body_src = "REMOVE /tmp WHERE age > 7";
    let parsed = parse_statement(insert_body_src).expect("parse insert body");
    let insert_body = PlanSpec::from_statement(parsed).canonical();

    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE ≡ INSERT: both normalise to one canonical span-normalised spec"
    );
}
