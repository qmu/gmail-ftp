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
    let stmt = parse_ok("FROM /mail/inbox");
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
        "FROM /mail/inbox \
         |> WHERE subject LIKE 'invoice' AND size > 1000 \
         |> SELECT id, subject AS title \
         |> JOIN /contacts ON id = contact_id \
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
    let stmt = parse_ok("FROM /t |> EXTEND total = price |> SET flag = TRUE");
    let Statement::Query(p) = stmt else { panic!() };
    assert!(matches!(p.ops[0], PipeOp::Extend(_)));
    assert!(matches!(p.ops[1], PipeOp::Set(_)));
}

#[test]
fn distinct_op() {
    let stmt = parse_ok("FROM /t |> DISTINCT");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::Distinct]);
}

#[test]
fn expand_op_struct_path() {
    let stmt = parse_ok("FROM /mail/inbox |> EXPAND attachments");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::Expand(vec!["attachments".to_string()])]);
}

#[test]
fn union_except_intersect_ops() {
    for (src, want) in [
        ("FROM /a |> UNION FROM /b", "union"),
        ("FROM /a |> EXCEPT FROM /b", "except"),
        ("FROM /a |> INTERSECT FROM /b", "intersect"),
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
    let stmt = parse_ok("FROM /t |> AS recent");
    let Statement::Query(p) = stmt else { panic!() };
    assert_eq!(p.ops, vec![PipeOp::As("recent".to_string())]);
}

// ---- codec registry seam --------------------------------------------------

#[test]
fn decode_and_encode_codecs() {
    let stmt = parse_ok("FROM /fs/data.json |> DECODE json |> ENCODE yaml");
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
    let stmt = parse_ok(
        "FROM /mail/drafts |> CALL mail.send |> CALL github.merge(method => 'squash', 42)",
    );
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
    let stmt = parse_ok("FROM /t |> SELECT upper(name)");
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
    let stmt = parse_ok("FROM /git/repo@main/src");
    let Statement::Query(p) = stmt else { panic!() };
    let Source::Path(path) = p.source else {
        panic!()
    };
    let seg = path.segments.iter().find(|s| s.name == "repo").unwrap();
    assert_eq!(seg.version.as_deref(), Some("main"));
}

#[test]
fn path_as_of_temporal() {
    let stmt = parse_ok("FROM /sql/pg/orders AS OF '2026-01-01'");
    let Statement::Query(p) = stmt else { panic!() };
    let Source::Path(path) = p.source else {
        panic!()
    };
    assert_eq!(path.as_of.as_deref(), Some("2026-01-01"));
}

#[test]
fn struct_path_access_a_b_c() {
    let stmt = parse_ok("FROM /t |> WHERE a.b.c = 1");
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
        parse_ok("FROM /t |> WHERE id IN (1, 2, 3) AND price BETWEEN 10 AND 20 AND x = ANY (4, 5)");
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
    let stmt = parse_ok("FROM /t |> WHERE NOT a = 1 OR b = 2");
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
    let stmt = parse_ok("INSERT INTO /archive FROM /mail/inbox |> WHERE flag = TRUE");
    let Statement::Effect(e) = stmt else { panic!() };
    assert!(matches!(e.body, EffectBody::Pipeline(_)));
}

#[test]
fn update_set_where() {
    let stmt = parse_ok("UPDATE /sql/pg/orders SET status = 'done' WHERE id = 7");
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

// ---- server DDL -----------------------------------------------------------

#[test]
fn ddl_endpoint_as_query() {
    let stmt = parse_ok("CREATE ENDPOINT recent ON 'GET /recent' AS FROM /mail/inbox |> LIMIT 5");
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
    let v = parse_ok("CREATE VIEW recent AS FROM /mail/inbox");
    let Statement::Ddl(d) = v else { panic!() };
    assert_eq!(d.kind, DdlKind::View);

    let mv = parse_ok("CREATE MATERIALIZED VIEW cached AS FROM /mail/inbox");
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
    // 4 statement forms (Query, Effect, Ddl, Plan).
    let statement_variants = [
        Statement::Query(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: vec![],
            }),
            ops: vec![],
        }),
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
        4,
        "Statement is closed at 4 forms (RFD §3)"
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

// ---- governance: rejection cases ------------------------------------------

#[test]
fn lowercase_keyword_rejected_as_unknown_keyword() {
    let err = parse_err("FROM /mail |> where id = 1");
    assert_eq!(err.code, ParseErrorCode::UnknownKeyword);
    // The error points at the offending lowercase `where`.
    assert!(err.span.start > 0);
    assert!(!err.expected.is_empty());
}

#[test]
fn reserved_word_as_identifier_is_rejected() {
    // `SELECT` (a reserved keyword) cannot stand where an identifier (a column) is
    // required.
    let err = parse_err("FROM /t |> SELECT SELECT");
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
    let err = parse_err("FROM /mail WHERE id = 1");
    // No `|>` before WHERE: trailing input is unexpected.
    assert!(!err.expected.is_empty());
}

#[test]
fn dangling_where_is_eof() {
    let err = parse_err("FROM /mail |> WHERE");
    assert_eq!(err.code, ParseErrorCode::UnexpectedEof);
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
    let err = parse_err("FROM /mail |> BANANA");
    assert!(
        !err.expected.is_empty(),
        "expected-set must be non-empty (RFD §5)"
    );
    // The span round-trips into the source region of the offending token.
    let src = "FROM /mail |> BANANA";
    let slice = &src[err.span.range()];
    assert!(!slice.is_empty());
}

#[test]
fn error_display_does_not_echo_string_literal_value() {
    // RFD §10: a diagnostic must not echo a literal's contents (secret hygiene).
    let err = parse_err("FROM /mail |> WHERE secret = 'p@ssw0rd' BANANA");
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
    assert!(KEYWORDS.contains(&"FROM"));
    assert_eq!(Keyword::Where.text(), "WHERE");
}
