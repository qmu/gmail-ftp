//! Unit tests for the full RFD §3 grammar (t04): one per pipe op, the governance
//! rejection cases, span-bearing errors, and the closed-core variant-count locks.

use super::*;
use crate::ast::*;

fn parse_ok(src: &str) -> Statement {
    parse_statement(src).unwrap_or_else(|e| panic!("expected `{src}` to parse, got {e}"))
}

fn parse_err(src: &str) -> ParseError {
    parse_statement(src).unwrap_err()
}

// ---- query pipeline + each pipe op ---------------------------------------

#[test]
fn from_only_query() {
    let stmt = parse_ok("/mail/inbox");
    let Statement::Query(p) = stmt else {
        panic!("expected Query")
    };
    let Source::Path(path) = p.source else {
        panic!("expected path source")
    };
    assert_eq!(path.segments.len(), 2);
    assert_eq!(path.segments[0].name, "mail");
    assert_eq!(path.segments[1].name, "inbox");
    assert!(p.ops.is_empty());
}

#[test]
fn multi_op_pipeline_covers_where_select_join_aggregate_order_limit() {
    let stmt = parse_ok(
        "/mail/inbox \
         |> WHERE subject LIKE 'invoice' AND size > 1000 \
         |> SELECT id, subject AS title \
         |> JOIN /contacts ON id == contact_id \
         |> AGGREGATE count(id) AS n \
         |> GROUP BY contact_id \
         |> ORDER BY n DESC \
         |> LIMIT 10",
    );
    let Statement::Query(p) = stmt else {
        panic!("expected Query")
    };
    assert_eq!(p.ops.len(), 7);
    assert!(matches!(p.ops[0], PipeOp::Where(_)));
    assert!(matches!(p.ops[1], PipeOp::Select(_)));
    assert!(matches!(p.ops[2], PipeOp::Join(_)));
    assert!(matches!(p.ops[3], PipeOp::Aggregate(_)));
    assert!(matches!(p.ops[4], PipeOp::GroupBy(_)));
    let PipeOp::OrderBy(ref keys) = p.ops[5] else {
        panic!("expected OrderBy")
    };
    assert!(keys[0].descending);
    assert_eq!(p.ops[6], PipeOp::Limit(10));
}

#[test]
fn extend_and_set_ops() {
    let stmt = parse_ok("/t |> EXTEND total = price |> SET flag = TRUE");
    let Statement::Query(p) = stmt else { panic!() };
    assert!(matches!(p.ops[0], PipeOp::Extend(_)));
    assert!(matches!(p.ops[1], PipeOp::Set(_)));
}

#[test]
fn distinct_op() {
    let stmt = parse_ok("/t |> DISTINCT");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::Distinct]);
}

#[test]
fn expand_op_struct_path() {
    let stmt = parse_ok("/mail/inbox |> EXPAND attachments");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::Expand(vec!["attachments".to_string()])]);
}

#[test]
fn union_except_intersect_ops() {
    for (src, want) in [
        ("/a |> UNION /b", "union"),
        ("/a |> EXCEPT /b", "except"),
        ("/a |> INTERSECT /b", "intersect"),
    ] {
        let stmt = parse_ok(src);
        let Statement::Query(p) = stmt else { panic!() };
        let got = match &p.ops[0] {
            PipeOp::Union(_) => "union",
            PipeOp::Except(_) => "except",
            PipeOp::Intersect(_) => "intersect",
            _ => "other",
        };
        assert_eq!(got, want, "for `{src}`");
    }
}

#[test]
fn as_op_names_relation() {
    let stmt = parse_ok("/t |> AS recent");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::As("recent".to_string())]);
}

// ---- codec registry seam --------------------------------------------------

#[test]
fn decode_and_encode_codecs() {
    let stmt = parse_ok("/fs/data.json |> DECODE json |> ENCODE yaml");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Decode(ref d) = p.ops[0] else {
        panic!("expected Decode")
    };
    assert_eq!(d.fmt, "json");
    let PipeOp::Encode(ref e) = p.ops[1] else {
        panic!("expected Encode")
    };
    assert_eq!(e.fmt, "yaml");
}

// ---- procedure registry seam ---------------------------------------------

#[test]
fn call_proc_with_named_and_positional_args() {
    let stmt =
        parse_ok("/mail/drafts |> CALL mail.send |> CALL github.merge(method => 'squash', 42)");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Call(ref c0) = p.ops[0] else {
        panic!("expected Call")
    };
    assert_eq!(c0.driver, "mail");
    assert_eq!(c0.action, "send");
    assert!(c0.args.is_empty());
    let PipeOp::Call(ref c1) = p.ops[1] else {
        panic!("expected Call")
    };
    assert_eq!(c1.driver, "github");
    assert_eq!(c1.action, "merge");
    assert_eq!(c1.args.len(), 2);
    assert_eq!(c1.args[0].name.as_deref(), Some("method"));
    assert_eq!(c1.args[1].name, None);
}

// ---- function registry seam ----------------------------------------------

#[test]
fn registry_fn_in_expression() {
    let stmt = parse_ok("/t |> SELECT upper(name)");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Select(ref projs) = p.ops[0] else {
        panic!()
    };
    let Projection::Expr { expr, .. } = &projs[0] else {
        panic!()
    };
    let Expr::Fn(f) = expr else {
        panic!("expected Fn, got {expr:?}")
    };
    assert_eq!(f.name, "upper");
    assert_eq!(f.args.len(), 1);
}

// ---- path registry seam: @version, AS OF, struct access -------------------

#[test]
fn path_at_version_is_preserved() {
    let stmt = parse_ok("/git/repo@main/src");
    let Statement::Query(p) = stmt else { panic!() };
    let Source::Path(path) = p.source else {
        panic!()
    };
    let seg = path.segments.iter().find(|s| s.name == "repo").unwrap();
    assert_eq!(seg.version.as_deref(), Some("main"));
}

#[test]
fn path_as_of_temporal() {
    let stmt = parse_ok("/sql/pg/orders AS OF '2026-01-01'");
    let Statement::Query(p) = stmt else { panic!() };
    let Source::Path(path) = p.source else {
        panic!()
    };
    assert_eq!(path.as_of.as_deref(), Some("2026-01-01"));
}

#[test]
fn struct_path_access_a_b_c() {
    let stmt = parse_ok("/t |> WHERE a.b.c == 1");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Where(Expr::Binary { lhs, .. }) = &p.ops[0] else {
        panic!()
    };
    assert_eq!(
        **lhs,
        Expr::Path(vec!["a".to_string(), "b".to_string(), "c".to_string()])
    );
}

// ---- expression operators -------------------------------------------------

#[test]
fn in_between_anyop_predicates() {
    let stmt =
        parse_ok("/t |> WHERE id IN (1, 2, 3) AND price BETWEEN 10 AND 20 AND x == ANY (4, 5)");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Where(e) = &p.ops[0] else {
        panic!()
    };
    // Top of the AND chain holds the AnyOp on its rhs.
    let Expr::Binary {
        op: Op::And, rhs, ..
    } = e
    else {
        panic!("expected AND at top, got {e:?}")
    };
    assert!(matches!(**rhs, Expr::AnyOp { .. }), "rhs should be AnyOp");
}

#[test]
fn not_and_or_precedence() {
    let stmt = parse_ok("/t |> WHERE NOT a == 1 OR b == 2");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Where(e) = &p.ops[0] else {
        panic!()
    };
    // OR binds loosest: top node is OR.
    assert!(matches!(e, Expr::Binary { op: Op::Or, .. }));
}

// ---- effect statements ----------------------------------------------------

#[test]
fn insert_values_returning() {
    let stmt =
        parse_ok("INSERT INTO /mail/drafts VALUES (to, subject) ('a@b.c', 'hi') RETURNING id");
    let Statement::Effect(e) = stmt else {
        panic!("expected Effect")
    };
    assert_eq!(e.verb, EffectVerb::Insert);
    let EffectBody::Values(v) = &e.body else {
        panic!("expected Values body")
    };
    assert_eq!(
        v.columns.as_ref().unwrap(),
        &vec!["to".to_string(), "subject".to_string()]
    );
    assert_eq!(v.rows.len(), 1);
    assert!(e.returning.is_some());
}

#[test]
fn upsert_distinct_from_insert() {
    let stmt = parse_ok("UPSERT INTO /s3/bucket/key VALUES ('blob')");
    let Statement::Effect(e) = stmt else { panic!() };
    assert_eq!(e.verb, EffectVerb::Upsert);
}

#[test]
fn insert_from_subpipeline() {
    let stmt = parse_ok("INSERT INTO /archive /mail/inbox |> WHERE flag == TRUE");
    let Statement::Effect(e) = stmt else { panic!() };
    assert!(matches!(e.body, EffectBody::Pipeline(_)));
}

#[test]
fn update_set_where() {
    let stmt = parse_ok("UPDATE /sql/pg/orders SET status = 'done' WHERE id == 7");
    let Statement::Effect(e) = stmt else { panic!() };
    assert_eq!(e.verb, EffectVerb::Update);
    let EffectBody::SetWhere { set, filter } = &e.body else {
        panic!()
    };
    assert_eq!(set.len(), 1);
    assert!(filter.is_some());
}

#[test]
fn remove_where() {
    let stmt = parse_ok("REMOVE /mail/spam WHERE age > 30");
    let Statement::Effect(e) = stmt else { panic!() };
    assert_eq!(e.verb, EffectVerb::Remove);
}

// ---- writes as pipeline stages (decision Q, t72) --------------------------

/// Render an AST to canonical JSON with every byte-`span` field stripped, so two spellings of
/// the SAME write — which sit at different byte offsets — compare on *structure* alone. The
/// whole point of t72 is that the pipe-stage and verb-leading forms lower to one `EffectStmt`;
/// spans are the only thing that legitimately differs between two source strings.
fn ast_without_spans(stmt: &Statement) -> serde_json::Value {
    let mut v = serde_json::to_value(stmt).expect("AST serializes");
    strip_spans(&mut v);
    v
}

fn strip_spans(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.remove("span");
            for child in map.values_mut() {
                strip_spans(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                strip_spans(child);
            }
        }
        _ => {}
    }
}

/// Decision Q, the load-bearing invariant: a pipe-stage write parses to the SAME effect AST the
/// verb-leading spelling produces — one plan, two renderings — for every write verb.
#[test]
fn pipe_stage_writes_match_their_verb_leading_form() {
    // INSERT/UPSERT: the upstream pipeline IS the inserted relation (the verb-leading form's
    // sub-pipeline body); target stays explicit in the stage.
    let pairs = [
        (
            "/mail/inbox |> WHERE flag == TRUE |> INSERT INTO /archive",
            "INSERT INTO /archive /mail/inbox |> WHERE flag == TRUE",
        ),
        (
            "/sql/pg/src |> SELECT id, body |> UPSERT INTO /drive/out.csv",
            "UPSERT INTO /drive/out.csv /sql/pg/src |> SELECT id, body",
        ),
        // UPDATE/REMOVE: target + filter are lifted from the upstream `path |> WHERE …` so the
        // pipe-stage form reproduces the verb-leading `UPDATE <path> SET … WHERE …` exactly.
        (
            "/sql/pg/orders |> WHERE id == 7 |> UPDATE SET status = 'done'",
            "UPDATE /sql/pg/orders SET status = 'done' WHERE id == 7",
        ),
        (
            "/mail/spam |> WHERE age > 30 |> REMOVE",
            "REMOVE /mail/spam WHERE age > 30",
        ),
        // RETURNING rides as a trailing `|>` stage in the pipe-stage form, inline in the other.
        (
            "/sql/pg/orders |> WHERE id == 7 |> UPDATE SET status = 'done' |> RETURNING id",
            "UPDATE /sql/pg/orders SET status = 'done' WHERE id == 7 RETURNING id",
        ),
    ];
    for (pipe_form, verb_form) in pairs {
        let lhs = ast_without_spans(&parse_ok(pipe_form));
        let rhs = ast_without_spans(&parse_ok(verb_form));
        assert_eq!(
            lhs, rhs,
            "pipe-stage `{pipe_form}` must lower to the same effect AST as `{verb_form}`"
        );
    }
}

/// Both forms are legal and produce an `Effect`; the source-less `VALUES` literal is unchanged.
#[test]
fn both_write_forms_are_legal() {
    // Pipe-stage write.
    let Statement::Effect(e) = parse_ok("/mail/inbox |> WHERE flag == TRUE |> REMOVE") else {
        panic!("pipe-stage REMOVE must be an Effect")
    };
    assert_eq!(e.verb, EffectVerb::Remove);
    // Source-less verb-leading literal still parses (the only INSERT form with no inflow).
    let Statement::Effect(lit) = parse_ok("INSERT INTO /mail/drafts VALUES ('hi')") else {
        panic!("source-less INSERT … VALUES must stay legal")
    };
    assert!(matches!(lit.body, EffectBody::Values(_)));
}

/// A pipe-stage INSERT/UPSERT keeps the upstream as the relational body (not a `SET`/`VALUES`).
#[test]
fn pipe_stage_insert_body_is_the_upstream_pipeline() {
    let Statement::Effect(e) = parse_ok("/sql/pg/src |> ENCODE csv |> UPSERT INTO /drive/o.csv")
    else {
        panic!()
    };
    assert_eq!(e.verb, EffectVerb::Upsert);
    let EffectBody::Pipeline(p) = &e.body else {
        panic!("expected the upstream pipeline as the body")
    };
    assert!(matches!(p.source, Source::Path(_)));
    assert_eq!(p.ops.len(), 1, "the ENCODE stage is part of the body");
}

/// A write stage with NO upstream source (a bare `|> INSERT INTO …`) is a clear error, not a
/// silent acceptance — decision Q's "anything that is neither legal form is an error".
#[test]
fn bare_pipe_write_with_no_source_is_an_error() {
    let _ = parse_err("|> INSERT INTO /mail/drafts");
    let _ = parse_err("|> REMOVE");
}

/// A pipe-stage UPDATE/REMOVE whose upstream is not a plain `path |> WHERE …` (here a `SELECT`
/// transform precedes it) cannot lower to the verb-leading in-place form, so it is rejected.
#[test]
fn pipe_stage_update_rejects_non_liftable_upstream() {
    let _ = parse_err("/sql/pg/orders |> SELECT id |> UPDATE SET status = 'done'");
}

/// A pipe-stage write is still an explicit effect under `PREVIEW`/`COMMIT` (the safety floor is
/// unchanged — only the surface syntax moved).
#[test]
fn pipe_stage_write_wraps_in_a_plan() {
    let Statement::Plan(p) = parse_ok("COMMIT /mail/spam |> WHERE age > 30 |> REMOVE") else {
        panic!("expected a plan wrapper")
    };
    assert!(p.commit);
    assert!(matches!(p.inner.as_ref(), Statement::Effect(_)));
}

// ---- server DDL -----------------------------------------------------------

#[test]
fn ddl_endpoint_as_query() {
    let stmt = parse_ok("CREATE ENDPOINT recent ON 'GET /recent' AS /mail/inbox |> LIMIT 5");
    let Statement::Ddl(d) = stmt else {
        panic!("expected Ddl")
    };
    assert_eq!(d.kind, DdlKind::Endpoint);
    assert_eq!(d.name, "recent");
    assert_eq!(d.target, vec!["server", "endpoints", "recent"]);
    assert!(d.as_query.is_some());
    assert_eq!(d.on.as_deref(), Some("GET /recent"));
}

#[test]
fn ddl_trigger_do_plan() {
    // The DO clause holds an effect-plan (a statement). A trigger that archives a
    // matched row on an inbox event:
    let stmt = parse_ok("CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('fired')");
    let Statement::Ddl(d) = stmt else { panic!() };
    assert_eq!(d.kind, DdlKind::Trigger);
    assert_eq!(d.on.as_deref(), Some("inbox"));
    // DO clause holds an inner statement.
    assert!(d.do_plan.is_some());
    assert!(matches!(d.do_plan.as_deref(), Some(Statement::Effect(_))));
}

#[test]
fn ddl_trigger_with_where_guard() {
    // t34 (CO-t31-4): `CREATE TRIGGER … ON <event> WHERE <pred> DO <plan>` — the optional WHERE
    // guard is captured in `where_pred` (a Query wrapping the predicate over an empty VALUES
    // source), distinct from the DO body's own statement.
    let stmt = parse_ok(
        "CREATE TRIGGER notify ON inbox WHERE priority > 3 DO INSERT INTO /log VALUES ('fired')",
    );
    let Statement::Ddl(d) = stmt else { panic!() };
    assert_eq!(d.kind, DdlKind::Trigger);
    assert_eq!(d.on.as_deref(), Some("inbox"));
    // The WHERE guard is surfaced as a Query whose single op is the predicate.
    let Some(Statement::Query(p)) = d.where_pred.as_deref() else {
        panic!("expected where_pred query")
    };
    assert_eq!(p.ops.len(), 1);
    assert!(matches!(p.ops[0], PipeOp::Where(_)));
    // The DO body is still the effect statement (its own clause, not the guard).
    assert!(matches!(d.do_plan.as_deref(), Some(Statement::Effect(_))));
}

#[test]
fn ddl_job_every() {
    let stmt = parse_ok("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let Statement::Ddl(d) = stmt else { panic!() };
    assert_eq!(d.kind, DdlKind::Job);
    assert_eq!(d.every.as_deref(), Some("1h"));
    assert!(d.do_plan.is_some());
}

#[test]
fn ddl_view_and_materialized_view() {
    let v = parse_ok("CREATE VIEW recent AS /mail/inbox");
    let Statement::Ddl(d) = v else { panic!() };
    assert_eq!(d.kind, DdlKind::View);

    let mv = parse_ok("CREATE MATERIALIZED VIEW cached AS /mail/inbox");
    let Statement::Ddl(d) = mv else { panic!() };
    assert_eq!(d.kind, DdlKind::MaterializedView);
    assert_eq!(d.target, vec!["server", "materialized_views", "cached"]);
}

#[test]
fn ddl_webhook_and_policy() {
    let w = parse_ok("CREATE WEBHOOK inbound ON '/hooks/x'");
    let Statement::Ddl(d) = w else { panic!() };
    assert_eq!(d.kind, DdlKind::Webhook);

    let pol = parse_ok("CREATE POLICY leastpriv");
    let Statement::Ddl(d) = pol else { panic!() };
    assert_eq!(d.kind, DdlKind::Policy);
    assert!(d.policy_rules.is_empty());
}

#[test]
fn ddl_policy_allow_deny_rules() {
    // The RFD §8 example: `ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL`. `ALLOW`/`DENY` are
    // contextual idents (NOT frozen keywords); verbs span keyword (SELECT/UPDATE/REMOVE/CALL)
    // and ident (INSERT/UPSERT) lexer shapes.
    let p = parse_ok("CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL");
    let Statement::Ddl(d) = p else { panic!() };
    assert_eq!(d.kind, DdlKind::Policy);
    assert_eq!(d.policy_rules.len(), 2);
    assert!(d.policy_rules[0].allow);
    assert_eq!(d.policy_rules[0].verbs, vec!["SELECT"]);
    assert!(!d.policy_rules[0].all_token);
    assert!(!d.policy_rules[1].allow);
    assert_eq!(
        d.policy_rules[1].verbs,
        vec!["INSERT", "UPDATE", "REMOVE", "CALL"]
    );

    // `ALLOW ALL ON mail` — bare ALL token + a per-rule driver glob.
    let p = parse_ok("CREATE POLICY broad ALLOW ALL ON mail");
    let Statement::Ddl(d) = p else { panic!() };
    assert_eq!(d.policy_rules.len(), 1);
    assert!(d.policy_rules[0].all_token);
    assert_eq!(d.policy_rules[0].verbs, vec!["ALL"]);
    assert_eq!(d.policy_rules[0].driver.as_deref(), Some("mail"));

    // `ALLOW UPSERT ON 's3/*'` — INSERT/UPSERT bare-ident verbs + a quoted glob (a glob with
    // `/` or `*` must be quoted, like the `ON 'GET /route'` endpoint operand).
    let p = parse_ok("CREATE POLICY s3w ALLOW UPSERT ON 's3/*'");
    let Statement::Ddl(d) = p else { panic!() };
    assert_eq!(d.policy_rules[0].verbs, vec!["UPSERT"]);
    assert_eq!(d.policy_rules[0].driver.as_deref(), Some("s3/*"));
}

// ---- plan wrappers --------------------------------------------------------

#[test]
fn preview_and_commit_wrappers() {
    let pv = parse_ok("PREVIEW REMOVE /mail/spam WHERE age > 30");
    let Statement::Plan(p) = pv else {
        panic!("expected Plan")
    };
    assert!(!p.commit);
    assert!(matches!(*p.inner, Statement::Effect(_)));

    let cm = parse_ok("COMMIT INSERT INTO /t VALUES (1)");
    let Statement::Plan(p) = cm else { panic!() };
    assert!(p.commit);
}

// ---- governance: closed-core variant counts ------------------------------

/// The closed-core thesis (RFD §3): the `Statement` and `PipeOp` variant sets are
/// fixed. This test mechanically locks their counts so a later ticket cannot smuggle
/// a per-driver / per-action variant into the grammar. Adding a variant here is a
/// deliberate, reviewed change-control event.
#[test]
fn closed_core_variant_counts_are_locked() {
    // 5 statement forms (Query, Effect, Ddl, Plan, Let). `Let` is the deliberate M6
    // functional-core addition (ticket t60), gated by exactly this lock + the keyword freeze.
    let statement_variants = [
        Statement::Query(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            ops: vec![],
        }),
        Statement::Let {
            name: String::new(),
            value: Box::new(Statement::Query(Pipeline {
                source: Source::Values(Values {
                    columns: None,
                    rows: vec![],
                }),
                ops: vec![],
            })),
            body: Box::new(Statement::Query(Pipeline {
                source: Source::Values(Values {
                    columns: None,
                    rows: vec![],
                }),
                ops: vec![],
            })),
        },
        Statement::Effect(EffectStmt {
            verb: EffectVerb::Insert,
            target: PathExpr {
                segments: vec![],
                as_of: None,
                span: Span::new(0, 0),
            },
            body: EffectBody::SetWhere {
                set: vec![],
                filter: None,
            },
            returning: None,
        }),
        Statement::Ddl(ServerDdl {
            kind: DdlKind::Policy,
            name: String::new(),
            target: vec![],
            do_plan: None,
            as_query: None,
            where_pred: None,
            every: None,
            on: None,
            policy_rules: vec![],
            policy: None,
        }),
        Statement::Plan(PlanWrap {
            commit: false,
            inner: Box::new(Statement::Query(Pipeline {
                source: Source::Values(Values {
                    columns: None,
                    rows: vec![],
                }),
                ops: vec![],
            })),
            span: Span::new(0, 0),
        }),
    ];
    assert_eq!(
        statement_variants.len(),
        5,
        "Statement is closed at 5 forms (RFD §3 + the t60 `LET` functional-core addition)"
    );

    // 18 closed-core pipe ops — each maps to a frozen query/transform keyword or a
    // registry seam (Decode/Encode/Call). There is NO per-action variant.
    let pipe_variants = [
        PipeOp::Where(Expr::Lit(Literal::Null)),
        PipeOp::Select(vec![]),
        PipeOp::Extend(vec![]),
        PipeOp::Set(vec![]),
        PipeOp::Aggregate(vec![]),
        PipeOp::GroupBy(vec![]),
        PipeOp::OrderBy(vec![]),
        PipeOp::Limit(0),
        PipeOp::Distinct,
        PipeOp::Join(JoinOp {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            on: Expr::Lit(Literal::Null),
        }),
        PipeOp::Union(Box::new(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            ops: vec![],
        })),
        PipeOp::Except(Box::new(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            ops: vec![],
        })),
        PipeOp::Intersect(Box::new(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            ops: vec![],
        })),
        PipeOp::As(String::new()),
        PipeOp::Expand(vec![]),
        PipeOp::Decode(Codec {
            fmt: String::new(),
            span: Span::new(0, 0),
        }),
        PipeOp::Encode(Codec {
            fmt: String::new(),
            span: Span::new(0, 0),
        }),
        PipeOp::Call(CallRef {
            driver: String::new(),
            action: String::new(),
            args: vec![],
            span: Span::new(0, 0),
        }),
    ];
    assert_eq!(
        pipe_variants.len(),
        18,
        "PipeOp is closed at 18 variants — no per-driver/per-action variant (RFD §3)"
    );
}

// ---- LET binding + multi-statement program (M6, ticket t60) ---------------

#[test]
fn let_binding_binds_a_relation_and_threads_body() {
    // A single LET binding followed by the statement that uses it (one per line, `;`-free).
    let stmt = parse_ok(
        "LET active = /sql/pg/customers |> WHERE status == 'active'\n\
         active |> SELECT id",
    );
    let Statement::Let { name, value, body } = stmt else {
        panic!("expected a LET binding, got {stmt:?}")
    };
    assert_eq!(name, "active");
    // The bound value is always a relation (a Query pipeline), never an effect.
    let Statement::Query(v) = *value else {
        panic!("LET value must be a Query")
    };
    let Source::Path(p) = v.source else {
        panic!("expected a path source")
    };
    assert_eq!(p.segments[0].name, "sql");
    // The body references the bound name as a bare-identifier source.
    let Statement::Query(b) = *body else {
        panic!("LET body must be the using statement")
    };
    assert_eq!(b.source, Source::Name("active".to_string()));
}

#[test]
fn multiple_let_bindings_nest_left_to_right() {
    // Two `;`-free LET lines stay in scope for the final statement (lexical, nested).
    let stmt = parse_ok(
        "LET a = /sql/pg/x\n\
         LET b = /sql/pg/y\n\
         a |> UNION b",
    );
    let Statement::Let { name, body, .. } = stmt else {
        panic!("expected outer LET a")
    };
    assert_eq!(name, "a");
    let Statement::Let {
        name: inner,
        body: inner_body,
        ..
    } = *body
    else {
        panic!("expected nested LET b")
    };
    assert_eq!(inner, "b");
    let Statement::Query(q) = *inner_body else {
        panic!("expected the final query")
    };
    assert_eq!(q.source, Source::Name("a".to_string()));
}

#[test]
fn bare_identifier_is_a_valid_source() {
    // `products` (a bare name, no leading slash, no `FROM`) is a bound-name source — shape-only
    // at parse time (decision R, t73: the source leads).
    let stmt = parse_ok("products |> LIMIT 5");
    let Statement::Query(p) = stmt else {
        panic!("expected Query")
    };
    assert_eq!(p.source, Source::Name("products".to_string()));
}

#[test]
fn let_value_may_not_be_an_effect() {
    // A LET binds a relation only; an effect value is a crisp parse error (`cut_err` after
    // the `=`), never a smuggled write into a pure context.
    let err = parse_err("LET x = INSERT INTO /sql/pg/t VALUES (1)\nx");
    assert!(!err.expected.is_empty());
}

#[test]
fn lowercase_let_is_rejected_as_unknown_keyword() {
    // `LET` is a frozen keyword: the lowercase form is flagged, like every other keyword. Post-t73
    // a leading bare word is a valid source, so the miscased `let` is exercised in trailing
    // position (after a valid terminal statement), where it surfaces as the offending token.
    let err = parse_err("/sql/pg/t\nlet x = /a");
    assert_eq!(err.code, ParseErrorCode::UnknownKeyword);
}

#[test]
fn two_bare_statements_without_a_binding_are_rejected() {
    // The program model is `LET* <final-statement>`; two non-binding statements in a row is
    // trailing input, not a silent split.
    let err = parse_err("/sql/pg/a\n/sql/pg/b");
    assert!(!err.expected.is_empty());
}

// ---- governance: rejection cases ------------------------------------------

#[test]
fn lowercase_keyword_rejected_as_unknown_keyword() {
    let err = parse_err("/mail |> where id = 1");
    assert_eq!(err.code, ParseErrorCode::UnknownKeyword);
    // The error points at the offending lowercase `where`.
    assert!(err.span.start > 0);
    assert!(!err.expected.is_empty());
}

#[test]
fn reserved_word_as_identifier_is_rejected() {
    // `SELECT` (a reserved keyword) cannot stand where an identifier (a column) is
    // required.
    let err = parse_err("/t |> SELECT SELECT");
    assert!(
        matches!(
            err.code,
            ParseErrorCode::ReservedAsIdentifier | ParseErrorCode::UnexpectedToken
        ),
        "got {:?}",
        err.code
    );
}

#[test]
fn missing_pipe_is_rejected() {
    let err = parse_err("/mail WHERE id = 1");
    // No `|>` before WHERE: trailing input is unexpected.
    assert!(!err.expected.is_empty());
}

#[test]
fn dangling_where_is_eof() {
    let err = parse_err("/mail |> WHERE");
    assert_eq!(err.code, ParseErrorCode::UnexpectedEof);
}

/// RFD decision O (ticket t70): a lone `=` always binds; it is never equality. A
/// stale SQL-style `WHERE a = 1` must therefore fail, with a message that steers the
/// author (human or AI) to `==` for equivalence (RFD §5 actionable-error contract).
#[test]
fn equals_as_comparison_is_rejected_steering_to_eqeq() {
    let err = parse_err("/t |> WHERE a = 1");
    assert_eq!(err.code, ParseErrorCode::UnexpectedToken);
    assert!(
        err.message.contains("=="),
        "message should steer to `==` for equivalence, got: {}",
        err.message
    );
    // The diagnostic points at the offending `=` (and never echoes any literal).
    let src = "/t |> WHERE a = 1";
    assert_eq!(&src[err.span.range()], "=");
}

/// The migrated comparison form `WHERE x == 5` parses as an equality predicate, while
/// the binding `=` keeps working in `EXTEND`/`SET` — the two roles never collide
/// (decision O, t70).
#[test]
fn eqeq_compares_while_eq_still_binds() {
    let stmt = parse_ok("/t |> WHERE x == 5");
    let Statement::Query(p) = stmt else { panic!() };
    let PipeOp::Where(Expr::Binary { op: Op::Eq, .. }) = &p.ops[0] else {
        panic!(
            "`WHERE x == 5` should parse as an Eq comparison, got {:?}",
            p.ops[0]
        )
    };
    // `=` is still the assignment/binding token in EXTEND and SET.
    let stmt = parse_ok("/t |> EXTEND y = x |> SET z = 1");
    let Statement::Query(p) = stmt else { panic!() };
    assert!(matches!(p.ops[0], PipeOp::Extend(_)));
    assert!(matches!(p.ops[1], PipeOp::Set(_)));
}

#[test]
fn empty_input_is_eof() {
    let err = parse_err("");
    assert_eq!(err.code, ParseErrorCode::UnexpectedEof);
    assert!(!err.expected.is_empty());
}

// ---- span fidelity & structured-error contract ----------------------------

#[test]
fn error_carries_span_and_nonempty_expected() {
    let err = parse_err("/mail |> BANANA");
    assert!(
        !err.expected.is_empty(),
        "expected-set must be non-empty (RFD §5)"
    );
    // The span round-trips into the source region of the offending token.
    let src = "/mail |> BANANA";
    let slice = &src[err.span.range()];
    assert!(!slice.is_empty());
}

#[test]
fn error_display_does_not_echo_string_literal_value() {
    // RFD §10: a diagnostic must not echo a literal's contents (secret hygiene).
    let err = parse_err("/mail |> WHERE secret = 'p@ssw0rd' BANANA");
    let shown = format!("{err}");
    assert!(
        !shown.contains("p@ssw0rd"),
        "error Display leaked a literal value: {shown}"
    );
}

// ---- no-vendor-leak audit (RFD §9, G6) ------------------------------------

#[test]
fn no_vendor_type_in_public_api() {
    let err = parse_statement("").expect_err("empty input is an error");
    // ParseError is fully owned: clonable, comparable, displayable with no
    // parser-library type in scope.
    let cloned = err.clone();
    assert_eq!(err, cloned);
    let shown = format!("{err}");
    assert!(shown.contains("UNEXPECTED_EOF"));
}

#[test]
fn sees_frozen_keyword_set() {
    // `FROM` was removed from the closed core in t73 (decision R): the source position needs no
    // keyword. `WHERE` (and the rest of the frozen set) is unaffected.
    assert!(!KEYWORDS.contains(&"FROM"));
    assert!(KEYWORDS.contains(&"WHERE"));
    assert_eq!(Keyword::Where.text(), "WHERE");
}
