//! Internal tests (t32): the request → bind → eval → encode pipeline driven IN-PROCESS (the
//! `oneshot` analogue — NO TCP, NO live network, NO credentials). All read I/O is an in-memory
//! fake [`qfs_exec::ReadDriver`]. Each test builds a [`HttpBinding`], reconciles it from a
//! synthetic [`qfs_server::ServerState`], and dispatches an owned [`HttpRequest`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, DriverId, Engine, NodeDesc, Path,
    PushdownProfile, Row, RowBatch, Schema, StatementSpec, Value,
};
use qfs_exec::{parse, ReadDriver, ReadRegistry};
use qfs_pushdown::ScanNode;
use qfs_server::{Binding, EndpointDef, ServerState};

use crate::handler::dispatch;
use crate::route::{compile_endpoint, RoutePattern};
use crate::{HttpBinding, HttpRequest, Method};

// ---- an in-memory fake `/mock` source: introspective (describe/pushdown) + read (scan) ----

struct FakeItems {
    rows: Vec<Row>,
}

fn items_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
    ])
}

impl FakeItems {
    fn new() -> Self {
        Self {
            rows: vec![
                Row::new(vec![Value::Int(1), Value::Text("alpha".into())]),
                Row::new(vec![Value::Int(2), Value::Text("beta".into())]),
                Row::new(vec![Value::Int(3), Value::Text("gamma".into())]),
            ],
        }
    }
}

impl qfs_core::Driver for FakeItems {
    fn mount(&self) -> &str {
        "/mock"
    }
    fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, items_schema()))
    }
    fn capabilities(&self, _path: &Path) -> Capabilities {
        // The SOURCE supports reads AND writes (a realistic backend). The read-only invariant
        // is enforced by the HTTP layer's policy gate, NOT by withholding source capability —
        // so a write-lowering endpoint genuinely produces a write Plan that the gate refuses.
        Capabilities::none().select().insert().update().remove()
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        // None: WHERE/LIMIT are local residuals; the scan over-returns and the engine re-filters.
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[derive(Default)]
struct NoopApplier;
impl qfs_core::PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_core::EffectNode,
    ) -> Result<qfs_core::AppliedEffect, qfs_core::ApplyError> {
        Ok(qfs_core::AppliedEffect::new(node.id, 0))
    }
}

#[async_trait::async_trait]
impl ReadDriver for FakeItems {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        // Honestly over-return ALL rows; the executor's residual trims to the bound result.
        Ok(RowBatch::new(items_schema(), self.rows.clone()))
    }
}

/// A counting fake that records how many times `scan` ran (asserts the live route was hit).
struct CountingItems {
    inner: FakeItems,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}

impl qfs_core::Driver for CountingItems {
    fn mount(&self) -> &str {
        "/mock"
    }
    fn describe(&self, p: &Path) -> Result<NodeDesc, CfsError> {
        self.inner.describe(p)
    }
    fn capabilities(&self, p: &Path) -> Capabilities {
        self.inner.capabilities(p)
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        self.inner.pushdown()
    }
    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[async_trait::async_trait]
impl ReadDriver for CountingItems {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.scan(scan).await
    }
}

// ---- fixtures: engine + reads + an EndpointDef from a query string ----

fn engine_with_mock() -> Arc<Engine> {
    let mut engine = Engine::new();
    engine.mounts.register(Arc::new(FakeItems::new())).unwrap();
    engine.codecs = qfs_core::CodecRegistry::with_builtins();
    Arc::new(engine)
}

fn reads_with_mock() -> Arc<ReadRegistry> {
    Arc::new(ReadRegistry::new().with(DriverId::new("mock"), Arc::new(FakeItems::new())))
}

/// Build an `EndpointDef` from a method/route and a query SOURCE string, storing the query as
/// the canonical span-normalised `StatementSpec` exactly as t31's DDL desugar does.
fn endpoint(name: &str, method: &str, route: &str, query_src: &str) -> EndpointDef {
    let stmt = parse(query_src).expect("endpoint query parses");
    let spec = StatementSpec::from_statement(stmt);
    EndpointDef {
        name: name.to_string(),
        method: method.to_string(),
        route: route.to_string(),
        query: qfs_server::StatementSource::new(spec.canonical()),
        policy: None,
    }
}

fn state_with(endpoints: Vec<EndpointDef>) -> ServerState {
    let mut state = ServerState::new();
    for ep in endpoints {
        state.endpoints.insert(ep.name.clone(), ep);
    }
    state
}

/// Reconcile a binding from a state and dispatch one request in-process, returning the response.
fn serve_once(binding: &HttpBinding, req: &HttpRequest) -> crate::HttpResponse {
    let router = binding.current_router();
    let ctx = binding.ctx();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        match router.match_request(&req.method, &req.path) {
            Some((route, path_params)) => dispatch(route, path_params, req, &ctx).await,
            None => crate::error::HttpError::NotFound.into_response(),
        }
    })
}

// ---------------------------------------------------------------------------
// Acceptance tests
// ---------------------------------------------------------------------------

#[test]
fn endpoint_registers_and_get_returns_200_json_of_matching_rows() {
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 10_000);
    // The route param `:p_id` is a DISTINCT identifier from the `id` column it filters on, so
    // the bare-identifier param convention is unambiguous (the LHS `id` stays a column, the RHS
    // `p_id` is the param slot the path segment binds into).
    let state = state_with(vec![endpoint(
        "items",
        "GET",
        "/items/:p_id",
        "FROM /mock/items |> WHERE id = p_id",
    )]);
    binding.reconcile(&state).unwrap();

    let resp = serve_once(&binding, &HttpRequest::new(Method::Get, "/items/2"));
    assert_eq!(resp.status, 200, "body: {}", resp.body_text());
    assert_eq!(resp.content_type, "application/json");
    let body = resp.body_text();
    // The WHERE id = 2 residual trims the over-returned 3 rows to the single matching row.
    assert!(body.contains("\"beta\""), "body was: {body}");
    assert!(
        !body.contains("\"alpha\""),
        "should not include id=1: {body}"
    );
    assert!(
        !body.contains("\"gamma\""),
        "should not include id=3: {body}"
    );
}

#[test]
fn write_endpoint_is_refused_at_registration_plan_assertion() {
    // An endpoint whose query lowers to a write effect (REMOVE) is refused at compile time.
    let engine = engine_with_mock();
    let def = endpoint(
        "purge",
        "POST",
        "/purge/:p_id",
        "REMOVE /mock/items WHERE id = p_id",
    );
    let result = compile_endpoint(&def, &engine, None);
    assert!(
        matches!(result, Err(crate::route::CompileError::Policy(_))),
        "a write-lowering endpoint must be refused by the read-only gate, got: {result:?}"
    );
}

#[test]
fn write_endpoint_is_allowed_when_a_policy_grants_it() {
    let engine = engine_with_mock();
    let def = endpoint(
        "purge",
        "POST",
        "/purge/:p_id",
        "REMOVE /mock/items WHERE id = p_id",
    );
    // t35: an explicit `ALLOW REMOVE` grants the irreversible REMOVE the endpoint lowers to
    // (a broad `ALLOW ALL` would NOT — irreversible strictness). The canonical rule string is
    // what `CREATE POLICY … ALLOW REMOVE` desugars into the `allow` array.
    let policy = qfs_server::PolicyDef {
        name: "writer".to_string(),
        handler: "purge".to_string(),
        allow: vec!["ALLOW REMOVE".to_string()],
    };
    let result = compile_endpoint(&def, &engine, Some(&policy));
    assert!(
        result.is_ok(),
        "a granting policy must open the write gate, got: {result:?}"
    );

    // Counter-case: a broad `ALLOW ALL` must NOT grant the irreversible REMOVE (it needs an
    // explicit ALLOW REMOVE) — the t35 irreversible-strictness rule.
    let broad = qfs_server::PolicyDef {
        name: "broad".to_string(),
        handler: "purge".to_string(),
        allow: vec!["ALLOW ALL".to_string()],
    };
    assert!(
        compile_endpoint(&def, &engine, Some(&broad)).is_err(),
        "ALLOW ALL must not silently grant irreversible REMOVE"
    );
}

#[test]
fn content_negotiation_json_default_and_csv_on_request() {
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 10_000);
    let state = state_with(vec![endpoint("all", "GET", "/all", "FROM /mock/items")]);
    binding.reconcile(&state).unwrap();

    // Default → JSON.
    let json = serve_once(&binding, &HttpRequest::new(Method::Get, "/all"));
    assert_eq!(json.status, 200);
    assert_eq!(json.content_type, "application/json");

    // ?format=csv → CSV.
    let csv_q = serve_once(
        &binding,
        &HttpRequest::new(Method::Get, "/all").with_query("format", "csv"),
    );
    assert_eq!(csv_q.status, 200);
    assert_eq!(csv_q.content_type, "text/csv");

    // Accept: text/csv → CSV.
    let csv_h = serve_once(
        &binding,
        &HttpRequest::new(Method::Get, "/all").with_header("Accept", "text/csv"),
    );
    assert_eq!(csv_h.status, 200);
    assert_eq!(csv_h.content_type, "text/csv");
    assert!(
        csv_h.body_text().contains("alpha"),
        "csv body: {}",
        csv_h.body_text()
    );
}

#[test]
fn missing_param_bind_error_is_400_naming_the_param() {
    // Assert the bind layer for a declared param with no source → 400 naming the param.
    let err = crate::QueryArgs::bind(
        &["id".to_string()],
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    let http: crate::HttpError = err.into();
    let resp = http.into_response();
    assert_eq!(resp.status, 400);
    let body = resp.body_text();
    assert!(
        body.contains("\"param\":\"id\""),
        "400 body names param: {body}"
    );
    assert!(body.contains("\"error\":\"bind\""), "body: {body}");
}

#[test]
fn extra_query_param_is_400_naming_the_param() {
    let mut query = BTreeMap::new();
    query.insert("surprise".to_string(), "1".to_string());
    let err = crate::QueryArgs::bind(&[], &BTreeMap::new(), &query, &BTreeMap::new()).unwrap_err();
    let resp: crate::HttpResponse = crate::HttpError::from(err).into_response();
    assert_eq!(resp.status, 400);
    assert!(
        resp.body_text().contains("\"param\":\"surprise\""),
        "{}",
        resp.body_text()
    );
}

#[test]
fn unknown_route_is_404() {
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 10_000);
    binding
        .reconcile(&state_with(vec![endpoint(
            "a",
            "GET",
            "/a",
            "FROM /mock/items",
        )]))
        .unwrap();
    let resp = serve_once(&binding, &HttpRequest::new(Method::Get, "/does-not-exist"));
    assert_eq!(resp.status, 404);
    assert!(
        resp.body_text().contains("\"error\":\"not_found\""),
        "{}",
        resp.body_text()
    );
}

#[test]
fn eval_error_is_422() {
    // An endpoint over a source with NO registered read driver: registration succeeds (it
    // lowers to a pure read), but evaluation fails at scan time → 422.
    let engine = engine_with_mock();
    let reads = Arc::new(ReadRegistry::new()); // no `mock` read driver registered
    let mut binding = HttpBinding::new(engine, reads, 10_000);
    binding
        .reconcile(&state_with(vec![endpoint(
            "a",
            "GET",
            "/a",
            "FROM /mock/items",
        )]))
        .unwrap();
    let resp = serve_once(&binding, &HttpRequest::new(Method::Get, "/a"));
    assert_eq!(resp.status, 422, "body: {}", resp.body_text());
    assert!(
        resp.body_text().contains("\"error\":\"eval\""),
        "{}",
        resp.body_text()
    );
}

#[test]
fn oversize_result_is_413() {
    // A 1-row cap with a 3-row result trips the bounded guard.
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 1);
    binding
        .reconcile(&state_with(vec![endpoint(
            "all",
            "GET",
            "/all",
            "FROM /mock/items",
        )]))
        .unwrap();
    let resp = serve_once(&binding, &HttpRequest::new(Method::Get, "/all"));
    assert_eq!(resp.status, 413, "body: {}", resp.body_text());
    assert!(
        resp.body_text().contains("\"error\":\"oversize\""),
        "{}",
        resp.body_text()
    );
}

#[test]
fn injection_path_param_binds_as_typed_value_with_identical_plan() {
    // A path param containing DSL-like text binds as a typed string literal; the bound query
    // AST is structurally IDENTICAL to a benign string bind (only the literal CONTENT differs).
    let route_query = "FROM /mock/items |> WHERE name = q_name";
    let stmt = parse(route_query).unwrap();

    let benign = crate::QueryArgs::new().with("q_name", Value::Text("alpha".to_string()));
    let malicious =
        crate::QueryArgs::new().with("q_name", Value::Text("'; REMOVE /mock/items".to_string()));

    let mut s_benign = stmt.clone();
    let mut s_malicious = stmt.clone();
    crate::rewrite::bind_params(&mut s_benign, &benign);
    crate::rewrite::bind_params(&mut s_malicious, &malicious);

    // Both bind to a single `Expr::Lit(Literal::Str(_))` node — same SHAPE. Strip the literal
    // string content (replace with a fixed marker) and the two ASTs are byte-identical: proof
    // that the malicious text is data, not parsed structure.
    let canon = |s: &qfs_parser::Statement| -> String {
        let json = serde_json::to_string(s).unwrap();
        // Replace the two distinct literal payloads with a marker so only the SHAPE remains.
        json.replace("'; REMOVE /mock/items", "MARKER")
            .replace("alpha", "MARKER")
    };
    assert_eq!(
        canon(&s_benign),
        canon(&s_malicious),
        "the malicious bind must produce a structurally identical plan (typed literal), not an \
         injected query"
    );

    // And the malicious bind genuinely lowers without introducing any write effect.
    let engine = engine_with_mock();
    let plan = qfs_exec::build_plan(&s_malicious, &engine).unwrap();
    assert!(
        plan.nodes()
            .iter()
            .all(|n| !crate::policy::is_write_effect(&n.kind)),
        "the malicious bind must not introduce any write effect"
    );
}

#[test]
fn hot_reload_adds_then_removes_a_route_via_reconcile_swap() {
    // A counting read driver proves the live route is served, and that a reconcile swap
    // re-binds the table: add /live (200), then remove it (404) — without restart.
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = Engine::new();
    engine.mounts.register(Arc::new(FakeItems::new())).unwrap();
    engine.codecs = qfs_core::CodecRegistry::with_builtins();
    let reads = Arc::new(ReadRegistry::new().with(
        DriverId::new("mock"),
        Arc::new(CountingItems {
            inner: FakeItems::new(),
            scans: Arc::clone(&scans),
        }),
    ));
    let mut binding = HttpBinding::new(Arc::new(engine), reads, 10_000);

    // Initially empty → 404.
    binding.reconcile(&ServerState::new()).unwrap();
    assert_eq!(
        serve_once(&binding, &HttpRequest::new(Method::Get, "/live")).status,
        404
    );

    // Add the endpoint (a committed /server/endpoints mutation) → reconcile → 200.
    binding
        .reconcile(&state_with(vec![endpoint(
            "live",
            "GET",
            "/live",
            "FROM /mock/items",
        )]))
        .unwrap();
    assert_eq!(
        serve_once(&binding, &HttpRequest::new(Method::Get, "/live")).status,
        200
    );
    assert!(
        scans.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the live route must have hit the read driver"
    );

    // Remove it (a committed REMOVE) → reconcile → 404 again.
    binding.reconcile(&ServerState::new()).unwrap();
    assert_eq!(
        serve_once(&binding, &HttpRequest::new(Method::Get, "/live")).status,
        404
    );
}

#[test]
fn route_pattern_extracts_named_params_and_rejects_mismatches() {
    let p = RoutePattern::parse("/items/:id/sub/{kind}");
    assert_eq!(p.param_names(), vec!["id".to_string(), "kind".to_string()]);
    let m = p.match_path("/items/42/sub/widget").unwrap();
    assert_eq!(m.get("id"), Some(&"42".to_string()));
    assert_eq!(m.get("kind"), Some(&"widget".to_string()));
    assert!(
        p.match_path("/items/42/sub").is_none(),
        "segment count mismatch"
    );
    assert!(
        p.match_path("/other/42/sub/widget").is_none(),
        "literal mismatch"
    );
}

#[test]
fn param_shadowing_a_referenced_column_is_refused_at_registration() {
    // The t32 security fix: a route param `:id` whose name collides with the `id` COLUMN the
    // query reads would make the typed-AST rewrite replace the wrong `Expr::Col` node and
    // collapse `WHERE id = id` into the tautology `WHERE 2 = 2` (access widening). This is
    // refused at registration with a structured error naming the param — no route is created.
    let engine = engine_with_mock();
    let def = endpoint(
        "shadow",
        "GET",
        "/items/:id",
        "FROM /mock/items |> WHERE id = id",
    );
    let result = compile_endpoint(&def, &engine, None);
    match result {
        Err(crate::route::CompileError::ParamShadowsColumn { param }) => {
            assert_eq!(param, "id", "the error must name the shadowing param");
        }
        other => panic!("a param shadowing a referenced column must be refused, got: {other:?}"),
    }

    // And via the binding's reconcile, the shadowing endpoint is SKIPPED (no live route): a GET
    // against it is 404, never a (silently widened) 200.
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 10_000);
    binding
        .reconcile(&state_with(vec![endpoint(
            "shadow",
            "GET",
            "/items/:id",
            "FROM /mock/items |> WHERE id = id",
        )]))
        .unwrap();
    assert_eq!(
        binding.current_router().len(),
        0,
        "no route may be registered"
    );
    assert_eq!(
        serve_once(&binding, &HttpRequest::new(Method::Get, "/items/2")).status,
        404
    );
}

#[test]
fn non_colliding_param_registers_and_serves() {
    // The benign counterpart: a route param DISTINCT from every queried column registers and
    // serves normally — the shadow gate does not false-refuse a correctly-authored endpoint.
    let engine = engine_with_mock();
    let def = endpoint(
        "ok",
        "GET",
        "/items/:p_id",
        "FROM /mock/items |> WHERE id = p_id",
    );
    assert!(
        compile_endpoint(&def, &engine, None).is_ok(),
        "a param distinct from every queried column must register"
    );

    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(), 10_000);
    binding
        .reconcile(&state_with(vec![endpoint(
            "ok",
            "GET",
            "/items/:p_id",
            "FROM /mock/items |> WHERE id = p_id",
        )]))
        .unwrap();
    assert_eq!(binding.current_router().len(), 1, "the route must register");
    let resp = serve_once(&binding, &HttpRequest::new(Method::Get, "/items/2"));
    assert_eq!(resp.status, 200, "body: {}", resp.body_text());
    assert!(
        resp.body_text().contains("\"beta\""),
        "body: {}",
        resp.body_text()
    );
}

// ---- t37: HTTP eval-error hygiene hardening (the t32 carry-over) ----

#[test]
fn eval_error_non_allowlisted_class_drops_raw_message() {
    use qfs_exec::{ErrorKind, ExecError};

    // A driver that carelessly stuffed an upstream secret into an Auth-class error message. With
    // the t37 hardening this NEVER reaches the caller-facing body: a non-allowlisted class is
    // reduced to `code` + a generic detail, so hygiene is UNCONDITIONAL (not driver-dependent).
    let leaky = ExecError::new(
        ErrorKind::Auth,
        "token_rejected",
        "upstream said: Bearer sk-LEAK-7f9c-PLANTED is invalid",
    );
    let problem = crate::HttpError::Eval(leaky).problem();
    assert_eq!(problem.error, "eval");
    assert!(
        !problem.detail.contains("sk-LEAK-7f9c-PLANTED"),
        "non-allowlisted eval error must not echo the raw message: {}",
        problem.detail
    );
    assert!(
        !problem.detail.contains("Bearer"),
        "the bearer token shape must be gone: {}",
        problem.detail
    );
    // The stable structured code is still surfaced for the caller/agent to branch on.
    assert!(
        problem.detail.starts_with("token_rejected:"),
        "the structured code is retained: {}",
        problem.detail
    );

    // An Internal class is likewise reduced to code + a generic detail.
    let internal = ExecError::new(
        ErrorKind::Internal,
        "bug",
        "panic at /home/secret/path token=X",
    );
    let p2 = crate::HttpError::Eval(internal).problem();
    assert!(!p2.detail.contains("token=X"), "{}", p2.detail);
    assert_eq!(p2.detail, "bug: an internal error occurred");
}

#[test]
fn eval_error_safe_class_keeps_structured_message() {
    use qfs_exec::{ErrorKind, ExecError};

    // The allowlisted/safe classes keep the executor's well-typed, secret-free message — these
    // are the diagnostics an agent branches on (e.g. which verb is unsupported). They never carry
    // a raw upstream string, so retaining them is safe.
    let cap = ExecError::new(
        ErrorKind::Capability,
        "unsupported_verb",
        "driver `mock` does not support REMOVE",
    );
    let problem = crate::HttpError::Eval(cap).problem();
    assert_eq!(problem.error, "eval");
    assert_eq!(problem.detail, "driver `mock` does not support REMOVE");
}
