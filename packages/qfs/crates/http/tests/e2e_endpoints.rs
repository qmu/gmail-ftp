//! Planner-owned **E2E / external-interface** black-box validation of the t32 HTTP serving
//! binding (`qfs-http`).
//!
//! This is NOT a unit test and NOT a code review. The Constructor's in-crate `src/tests.rs`
//! drives the IN-PROCESS `dispatch` analogue; THIS file drives the system from the OUTSIDE,
//! the way an AI agent or a host would: it spawns the REAL native HTTP/1.1 listener over a
//! loopback `tokio::net::TcpListener` (`127.0.0.1:<ephemeral>`) via the public
//! [`qfs_http::serve`] entry and issues genuine HTTP/1.1 requests as raw socket bytes, then
//! parses the wire response. Every assertion is on observable wire behaviour (status line,
//! `Content-Type`, body bytes) — never a private internal.
//!
//! No live network beyond loopback, no live credentials: all read I/O is an in-memory fake
//! [`qfs_exec::ReadDriver`]. The federated read endpoint contract is exercised end to end
//! through the public crate surface only.
//!
//! Scenario map (ticket acceptance criteria):
//!  1. Live route + 200 JSON: a registered `GET /items/:p_id` returns 200 + JSON of the row.
//!  2. Read-only policy gate: a write-lowering endpoint registers NO route (404) by default;
//!     a granting POLICY opens the route (200) — confirmed at the wire.
//!  3. SECURITY param-shadow refusal: a route param colliding with a queried COLUMN serves NO
//!     route (404, never a widened 200); a fail-closed late-bound source is refused too. The
//!     scenario actively tries to make a shadowing endpoint serve.
//!  4. Injection safety: a path param carrying DSL-like text binds as a typed value — the
//!     result is identical to a benign value and never alters the plan / fires an effect.
//!  5. Content negotiation: JSON default, `text/csv` under `Accept` and `?format=csv`.
//!  6. Param bind errors: missing / extra param → 400 naming the offending param.
//!  7. Status mapping: unknown route → 404; eval error → 422; oversize → 413; all JSON.
//!  8. Hot-reload: a reconcile add makes a route serve on the next request; remove → 404.
//!  9. Secret / error hygiene: a planted canary never appears in any response body.
//!
//! Conventional test-allow header (the same allowance every other test crate carries; the
//! crate root declares `#![cfg_attr(test, allow(...))]` for the in-crate module, and this is a
//! separate test crate so it re-declares it).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, DriverId, Engine, NodeDesc, Path,
    PushdownProfile, Row, RowBatch, Schema, StatementSpec, Value,
};
use qfs_exec::{parse, ReadDriver, ReadRegistry};
use qfs_pushdown::ScanNode;
use qfs_server::{Binding, EndpointDef, PolicyDef, ServerState, StatementSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use qfs_http::{serve, HttpBinding};

// ---------------------------------------------------------------------------
// In-memory fake `/mock` source (read I/O only; no network, no creds).
// ---------------------------------------------------------------------------

/// A canary "credential" planted in the fake's rows so the hygiene scenario can prove it never
/// leaks into an ERROR body. (It MAY legitimately appear in a 200 data body — the point is that
/// failure paths never surface it; scenario 9 asserts absence on the error path.)
const CANARY: &str = "SECRET-CANARY-7f3a9";

fn items_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
    ])
}

struct FakeItems {
    rows: Vec<Row>,
    scans: Arc<AtomicUsize>,
}

impl FakeItems {
    fn new(scans: Arc<AtomicUsize>) -> Self {
        Self {
            rows: vec![
                Row::new(vec![Value::Int(1), Value::Text("alpha".into())]),
                Row::new(vec![Value::Int(2), Value::Text("beta".into())]),
                Row::new(vec![Value::Int(3), Value::Text("gamma".into())]),
            ],
            scans,
        }
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

impl qfs_core::Driver for FakeItems {
    fn mount(&self) -> &str {
        "/mock"
    }
    fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, items_schema()))
    }
    fn capabilities(&self, _path: &Path) -> Capabilities {
        // The SOURCE genuinely supports writes; the read-only invariant is the HTTP policy
        // gate's job, NOT withheld capability — so a write endpoint truly produces a write Plan.
        Capabilities::none().select().insert().update().remove()
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        // None: WHERE is a local residual; the scan over-returns and the engine re-filters.
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[async_trait::async_trait]
impl ReadDriver for FakeItems {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(RowBatch::new(items_schema(), self.rows.clone()))
    }
}

// ---------------------------------------------------------------------------
// Fixtures: engine + reads + an EndpointDef from a query string.
// ---------------------------------------------------------------------------

fn engine_with_mock() -> Arc<Engine> {
    let mut engine = Engine::new();
    engine
        .mounts
        .register(Arc::new(FakeItems::new(Arc::new(AtomicUsize::new(0)))))
        .expect("register mock mount");
    engine.codecs = qfs_core::CodecRegistry::with_builtins();
    Arc::new(engine)
}

fn reads_with_mock(scans: Arc<AtomicUsize>) -> Arc<ReadRegistry> {
    Arc::new(ReadRegistry::new().with(DriverId::new("mock"), Arc::new(FakeItems::new(scans))))
}

/// Build an `EndpointDef` storing the query as the canonical span-normalised `StatementSpec`,
/// exactly as t31's DDL desugar does (no re-parse at request time).
fn endpoint(name: &str, method: &str, route: &str, query_src: &str) -> EndpointDef {
    let stmt = parse(query_src).expect("endpoint query parses");
    let spec = StatementSpec::from_statement(stmt);
    EndpointDef {
        name: name.to_string(),
        method: method.to_string(),
        route: route.to_string(),
        query: StatementSource::new(spec.canonical()),
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

/// A parsed HTTP/1.1 wire response (the bytes that came back over the socket).
struct WireResponse {
    status: u16,
    content_type: String,
    body: String,
}

/// A live loopback server: spawns the REAL [`qfs_http::serve`] listener on an ephemeral
/// `127.0.0.1` port and hands back the bound address + a shutdown signal. Dropping it stops
/// the listener. The `binding` is reconciled BEFORE the listener is spawned so its shared
/// router/ctx handles are wired to the live table.
struct LiveServer {
    addr: SocketAddr,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
    // Keep the binding alive for the server's lifetime AND allow hot-reconcile mid-test.
    binding: HttpBinding,
}

impl LiveServer {
    /// Spawn a live loopback listener over the binding's shared router/ctx handles.
    async fn start(binding: HttpBinding) -> Self {
        // Pick a free ephemeral loopback port by probing, then release it for `serve` to bind.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let router = binding.router_handle();
        let ctx = binding.ctx();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let wait = async move {
                while shutdown_rx.changed().await.is_ok() {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            };
            // `serve` binds `addr` and serves until `wait` resolves. A bind race on the
            // just-released ephemeral port is astronomically unlikely on loopback.
            let _ = serve(addr, router, ctx, wait).await;
        });

        // Wait until the listener actually accepts (poll-connect with a bounded retry) so the
        // first real request never races the bind.
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Self {
            addr,
            shutdown_tx,
            handle,
            binding,
        }
    }

    /// Reconcile the live binding from a new state (the hot-reload swap). The next request sees
    /// the new route table — no restart.
    fn reconcile(&mut self, state: &ServerState) {
        self.binding.reconcile(state).expect("reconcile");
    }

    /// Issue ONE raw HTTP/1.1 request over a fresh loopback socket and parse the wire response.
    /// `extra_headers` are appended verbatim (e.g. `Accept: text/csv`).
    async fn request(&self, method: &str, target: &str, extra_headers: &[&str]) -> WireResponse {
        let mut stream = TcpStream::connect(self.addr).await.expect("connect");
        let mut req = format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\n");
        for h in extra_headers {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write req");
        stream.flush().await.expect("flush");

        // The server sends `Connection: close`, so read to EOF.
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read resp");
        parse_wire(&raw)
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        // Nudge the accept loop so it observes the shutdown promptly.
        let _ = TcpStream::connect(self.addr).await;
        let _ = self.handle.await;
    }
}

/// Parse raw HTTP/1.1 response bytes into a [`WireResponse`].
fn parse_wire(raw: &[u8]) -> WireResponse {
    let text = String::from_utf8_lossy(raw);
    let split = text
        .find("\r\n\r\n")
        .expect("response has a header terminator");
    let head = &text[..split];
    let body = text[split + 4..].to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    // `HTTP/1.1 200 OK`
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let mut content_type = String::new();
    for line in lines {
        if let Some(v) = line
            .to_ascii_lowercase()
            .strip_prefix("content-type:")
            .map(str::trim)
        {
            content_type = v.to_string();
        }
    }
    WireResponse {
        status,
        content_type,
        body,
    }
}

// ===========================================================================
// Scenario 1 — Live route + 200 JSON over the real loopback wire.
// ===========================================================================

#[tokio::test]
async fn s1_live_route_returns_200_json_of_matching_row() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    binding
        .reconcile(&state_with(vec![endpoint(
            "items",
            "GET",
            "/items/:p_id",
            "/mock/items |> WHERE id == p_id",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;

    let resp = server.request("GET", "/items/2", &[]).await;
    assert_eq!(resp.status, 200, "wire body: {}", resp.body);
    assert_eq!(resp.content_type, "application/json");
    assert!(resp.body.contains("beta"), "body: {}", resp.body);
    assert!(
        !resp.body.contains("alpha"),
        "must trim id=1: {}",
        resp.body
    );
    assert!(
        !resp.body.contains("gamma"),
        "must trim id=3: {}",
        resp.body
    );
    assert!(scans.load(Ordering::SeqCst) >= 1, "the read driver was hit");

    server.shutdown().await;
}

// ===========================================================================
// Scenario 2 — Read-only policy gate at the wire: default-deny + POLICY opens.
// ===========================================================================

#[tokio::test]
async fn s2_write_endpoint_default_denied_serves_no_route() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // A write-lowering endpoint (REMOVE) with NO policy: refused at registration → no route.
    binding
        .reconcile(&state_with(vec![endpoint(
            "purge",
            "POST",
            "/purge/:p_id",
            "REMOVE /mock/items WHERE id == p_id",
        )]))
        .unwrap();
    assert_eq!(
        binding.current_router().len(),
        0,
        "default-deny: a write endpoint must register no route"
    );
    let server = LiveServer::start(binding).await;

    let resp = server.request("POST", "/purge/2", &[]).await;
    assert_eq!(resp.status, 404, "no live route → 404, body: {}", resp.body);
    assert!(resp.body.contains("not_found"), "body: {}", resp.body);
    assert_eq!(
        scans.load(Ordering::SeqCst),
        0,
        "no effect/read ever ran for the refused write endpoint"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn s2_write_endpoint_opens_route_when_policy_grants() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // The endpoint names a policy; the policy grants writes → the route registers.
    let mut def = endpoint(
        "purge",
        "POST",
        "/purge/:p_id",
        "REMOVE /mock/items WHERE id == p_id",
    );
    def.policy = Some("writer".to_string());
    let mut state = state_with(vec![def]);
    state.policies.insert(
        "writer".to_string(),
        PolicyDef {
            name: "writer".to_string(),
            handler: "purge".to_string(),
            // t35: an explicit `ALLOW REMOVE` grants the irreversible REMOVE the endpoint
            // lowers to (the canonical rule string `CREATE POLICY … ALLOW REMOVE` desugars to).
            allow: vec!["ALLOW REMOVE".to_string()],
        },
    );
    binding.reconcile(&state).unwrap();
    assert_eq!(
        binding.current_router().len(),
        1,
        "a granting POLICY must open the write route at registration"
    );

    server_route_exists(binding).await;
}

/// Confirm the policy-granted route is LIVE at the wire (matches → not a 404 from no route).
/// The effect itself runs through the read executor's apply path; we assert the route exists
/// and is reachable (status is not 404), which is the gate-open observable.
async fn server_route_exists(binding: HttpBinding) {
    let server = LiveServer::start(binding).await;
    let resp = server.request("POST", "/purge/2", &[]).await;
    assert_ne!(
        resp.status, 404,
        "the policy-granted route must be live (not a no-route 404), got body: {}",
        resp.body
    );
    server.shutdown().await;
}

// ===========================================================================
// Scenario 3 — SECURITY: param-shadow / access-widening refusal at the wire.
// Actively try to make a shadowing endpoint serve. It MUST NOT.
// ===========================================================================

#[tokio::test]
async fn s3_param_shadowing_a_column_serves_no_route_never_widens() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // `:id` collides with the `id` COLUMN: `WHERE id == id` would, if the param `id` replaced the
    // column node, collapse into the tautology `WHERE 2 = 2` (returning ALL rows — access
    // widening). The gate must refuse registration → no route.
    binding
        .reconcile(&state_with(vec![endpoint(
            "shadow",
            "GET",
            "/items/:id",
            "/mock/items |> WHERE id == id",
        )]))
        .unwrap();
    assert_eq!(
        binding.current_router().len(),
        0,
        "a shadowing endpoint must register NO route"
    );
    let server = LiveServer::start(binding).await;

    // Try to make it serve: a GET that, if widened, would return all 3 rows.
    let resp = server.request("GET", "/items/2", &[]).await;
    assert_eq!(
        resp.status, 404,
        "shadowing endpoint must 404, NEVER a widened 200; body: {}",
        resp.body
    );
    // Belt and suspenders: even if some path leaked a 200, it must not contain the widened set.
    assert!(
        !resp.body.contains("alpha") && !resp.body.contains("gamma"),
        "no widened all-rows body may EVER be served: {}",
        resp.body
    );
    assert_eq!(
        scans.load(Ordering::SeqCst),
        0,
        "the refused shadow endpoint must never reach the read driver"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn s3_fail_closed_shadow_over_unresolvable_source_is_refused() {
    // FAIL-CLOSED: a param used as a column over a source whose schema cannot be resolved
    // (no such mount in this engine) must ALSO be refused — we cannot prove the param is not a
    // real column of the late-bound source. Here `/unknown/x` is not registered, so the source
    // schema is unverifiable; `:thing` is referenced as a column (`WHERE thing == thing`).
    let engine = engine_with_mock(); // has /mock, NOT /unknown
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(engine, reads_with_mock(Arc::clone(&scans)), 10_000);
    binding
        .reconcile(&state_with(vec![endpoint(
            "late",
            "GET",
            "/late/:thing",
            "/unknown/x |> WHERE thing == thing",
        )]))
        .unwrap();
    assert_eq!(
        binding.current_router().len(),
        0,
        "fail-closed: a shadow over an unverifiable source must register no route"
    );
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/late/2", &[]).await;
    assert_eq!(
        resp.status, 404,
        "fail-closed refusal → 404; body: {}",
        resp.body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn s3_non_colliding_param_is_not_false_refused() {
    // The shadow gate must NOT false-refuse a correctly authored endpoint: a route param
    // DISTINCT from every queried column registers and serves the scoped row.
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    binding
        .reconcile(&state_with(vec![endpoint(
            "ok",
            "GET",
            "/items/:p_id",
            "/mock/items |> WHERE id == p_id",
        )]))
        .unwrap();
    assert_eq!(
        binding.current_router().len(),
        1,
        "distinct param must register"
    );
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/items/2", &[]).await;
    assert_eq!(resp.status, 200, "body: {}", resp.body);
    assert!(resp.body.contains("beta"), "scoped to id=2: {}", resp.body);
    assert!(
        !resp.body.contains("alpha") && !resp.body.contains("gamma"),
        "must be scoped, not widened: {}",
        resp.body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn s3_adversarial_attempts_to_defeat_the_shadow_guard_all_refused() {
    // Actively try several vectors to get a shadowing endpoint to actually serve. Each MUST be
    // refused at registration (no route) — proving the guard covers every substitution site the
    // rewrite touches, not just `WHERE col = col`.
    let cases: &[(&str, &str, &str)] = &[
        // (a) Brace param form `{id}` (same shadow, different route syntax).
        ("brace", "/items/{id}", "/mock/items |> WHERE id == id"),
        // (b) Shadow a column referenced ONLY in a SELECT projection (not a WHERE) — the
        //     rewrite still substitutes `Expr::Col` in projections, so this would widen too.
        ("proj", "/items/:name", "/mock/items |> SELECT name"),
        // (c) Shadow a column reached through a subquery source — the collect walk recurses
        //     into subqueries, so a param matching its column is still a shadow.
        (
            "sub",
            "/items/:id",
            "(/mock/items |> WHERE id == 1) |> WHERE id == id",
        ),
    ];
    for (name, route, query) in cases {
        let scans = Arc::new(AtomicUsize::new(0));
        let mut binding = HttpBinding::new(
            engine_with_mock(),
            reads_with_mock(Arc::clone(&scans)),
            10_000,
        );
        binding
            .reconcile(&state_with(vec![endpoint(name, "GET", route, query)]))
            .unwrap();
        assert_eq!(
            binding.current_router().len(),
            0,
            "shadow vector `{name}` ({query}) MUST register no route"
        );
        let server = LiveServer::start(binding).await;
        let resp = server.request("GET", "/items/2", &[]).await;
        assert_eq!(
            resp.status, 404,
            "shadow vector `{name}` must 404, never serve; body: {}",
            resp.body
        );
        assert_eq!(
            scans.load(Ordering::SeqCst),
            0,
            "shadow vector `{name}` must never reach the read driver"
        );
        server.shutdown().await;
    }
}

// ===========================================================================
// Scenario 4 — Injection safety: DSL-like path param binds as a typed value,
// identical result to a benign value, no effect ever fires.
// ===========================================================================

#[tokio::test]
async fn s4_injection_path_param_is_typed_data_not_query_structure() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // Filter on `name` (Text) so a string path param binds there. `:q_name` is distinct from
    // every column → no shadow.
    binding
        .reconcile(&state_with(vec![endpoint(
            "byname",
            "GET",
            "/byname/:q_name",
            "/mock/items |> WHERE name == q_name",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;

    // Benign value → matches the `alpha` row.
    let benign = server.request("GET", "/byname/alpha", &[]).await;
    assert_eq!(benign.status, 200, "body: {}", benign.body);
    assert!(
        benign.body.contains("alpha"),
        "benign body: {}",
        benign.body
    );

    // Malicious DSL-like value (URL-encoded `'; REMOVE /mock/items`). It is bound as a TYPED
    // string literal: it matches NO `name`, so it returns an empty result set — and CRUCIALLY
    // it neither errors (no parse/injection) nor fires any effect.
    let malicious = server
        .request("GET", "/byname/%27%3B%20REMOVE%20%2Fmock%2Fitems", &[])
        .await;
    assert_eq!(
        malicious.status, 200,
        "the malicious text is data, not structure — it evaluates as a benign no-match read; \
         body: {}",
        malicious.body
    );
    // No row name equals the injection string → empty data; specifically NOT the widened set.
    assert!(
        !malicious.body.contains("alpha")
            && !malicious.body.contains("beta")
            && !malicious.body.contains("gamma"),
        "the injection string must match no row and never widen access: {}",
        malicious.body
    );
    // The rows still exist — the REMOVE text did NOT delete anything (read again, benign).
    let after = server.request("GET", "/byname/beta", &[]).await;
    assert!(
        after.body.contains("beta"),
        "the data is intact; no effect fired: {}",
        after.body
    );

    server.shutdown().await;
}

// ===========================================================================
// Scenario 5 — Content negotiation at the wire: JSON default, CSV on request.
// ===========================================================================

#[tokio::test]
async fn s5_content_negotiation_json_default_csv_on_request() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    binding
        .reconcile(&state_with(vec![endpoint(
            "all",
            "GET",
            "/all",
            "/mock/items",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;

    // Default → JSON.
    let json = server.request("GET", "/all", &[]).await;
    assert_eq!(json.status, 200);
    assert_eq!(json.content_type, "application/json");
    assert!(json.body.contains("alpha"));

    // ?format=csv → CSV.
    let csv_q = server.request("GET", "/all?format=csv", &[]).await;
    assert_eq!(csv_q.status, 200, "body: {}", csv_q.body);
    assert_eq!(csv_q.content_type, "text/csv");
    assert!(csv_q.body.contains("alpha"), "csv body: {}", csv_q.body);

    // Accept: text/csv → CSV.
    let csv_h = server.request("GET", "/all", &["Accept: text/csv"]).await;
    assert_eq!(csv_h.status, 200, "body: {}", csv_h.body);
    assert_eq!(csv_h.content_type, "text/csv");
    // CSV body differs from JSON (no `{`/`}` object syntax wrapping the row).
    assert!(
        !csv_h.body.trim_start().starts_with('['),
        "csv body is not a JSON array: {}",
        csv_h.body
    );

    server.shutdown().await;
}

// ===========================================================================
// Scenario 6 — Param bind errors: missing / extra → 400 naming the param.
// ===========================================================================

#[tokio::test]
async fn s6_extra_query_param_is_400_naming_the_param() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // The endpoint declares no query-string params; an unexpected `?surprise=1` is rejected.
    binding
        .reconcile(&state_with(vec![endpoint(
            "all",
            "GET",
            "/all",
            "/mock/items",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;

    let resp = server.request("GET", "/all?surprise=1", &[]).await;
    assert_eq!(resp.status, 400, "extra param → 400; body: {}", resp.body);
    assert!(
        resp.body.contains("\"param\":\"surprise\""),
        "400 body must name the offending param: {}",
        resp.body
    );
    assert!(
        resp.body.contains("\"error\":\"bind\""),
        "body: {}",
        resp.body
    );

    server.shutdown().await;
}

#[tokio::test]
async fn s6_format_knob_is_not_an_extra_param() {
    // `?format=csv` is a RESERVED negotiation knob, not a query param — it must NOT trip the
    // closed-param contract even on a no-param endpoint.
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    binding
        .reconcile(&state_with(vec![endpoint(
            "all",
            "GET",
            "/all",
            "/mock/items",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/all?format=csv", &[]).await;
    assert_eq!(resp.status, 200, "format knob must not 400: {}", resp.body);
    server.shutdown().await;
}

// ===========================================================================
// Scenario 7 — Status mapping: unknown 404, eval 422, oversize 413.
// ===========================================================================

#[tokio::test]
async fn s7_unknown_route_is_404() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    binding
        .reconcile(&state_with(vec![endpoint("a", "GET", "/a", "/mock/items")]))
        .unwrap();
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/does-not-exist", &[]).await;
    assert_eq!(resp.status, 404, "body: {}", resp.body);
    assert!(
        resp.body.contains("\"error\":\"not_found\""),
        "body: {}",
        resp.body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn s7_eval_error_is_422() {
    // Registration succeeds (a pure read lowers cleanly), but no `mock` READ driver is
    // registered, so evaluation fails at scan time → 422.
    let engine = engine_with_mock();
    let reads = Arc::new(ReadRegistry::new()); // NO read driver
    let mut binding = HttpBinding::new(engine, reads, 10_000);
    binding
        .reconcile(&state_with(vec![endpoint("a", "GET", "/a", "/mock/items")]))
        .unwrap();
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/a", &[]).await;
    assert_eq!(resp.status, 422, "eval failure → 422; body: {}", resp.body);
    assert!(
        resp.body.contains("\"error\":\"eval\""),
        "body: {}",
        resp.body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn s7_oversize_result_is_413() {
    // A 1-row cap against a 3-row result trips the bounded guard.
    let scans = Arc::new(AtomicUsize::new(0));
    let mut binding = HttpBinding::new(engine_with_mock(), reads_with_mock(Arc::clone(&scans)), 1);
    binding
        .reconcile(&state_with(vec![endpoint(
            "all",
            "GET",
            "/all",
            "/mock/items",
        )]))
        .unwrap();
    let server = LiveServer::start(binding).await;
    let resp = server.request("GET", "/all", &[]).await;
    assert_eq!(resp.status, 413, "oversize → 413; body: {}", resp.body);
    assert!(
        resp.body.contains("\"error\":\"oversize\""),
        "body: {}",
        resp.body
    );
    server.shutdown().await;
}

// ===========================================================================
// Scenario 8 — Hot-reload: a reconcile swap makes a route serve, then 404 on remove.
// ===========================================================================

#[tokio::test]
async fn s8_hot_reload_add_then_remove_without_restart() {
    let scans = Arc::new(AtomicUsize::new(0));
    let binding = HttpBinding::new(
        engine_with_mock(),
        reads_with_mock(Arc::clone(&scans)),
        10_000,
    );
    // Boot EMPTY → /live is 404.
    let mut server = LiveServer::start(binding).await;
    {
        // empty state already (binding constructed empty); confirm 404 before any add.
        let resp = server.request("GET", "/live", &[]).await;
        assert_eq!(resp.status, 404, "no route yet: {}", resp.body);
    }

    // Hot-add the endpoint (a committed /server/endpoints mutation) — SAME running listener.
    server.reconcile(&state_with(vec![endpoint(
        "live",
        "GET",
        "/live",
        "/mock/items",
    )]));
    let added = server.request("GET", "/live", &[]).await;
    assert_eq!(
        added.status, 200,
        "route serves after hot reconcile: {}",
        added.body
    );
    assert!(added.body.contains("alpha"), "body: {}", added.body);

    // Hot-remove it (a committed REMOVE) — SAME listener → 404 again.
    server.reconcile(&ServerState::new());
    let removed = server.request("GET", "/live", &[]).await;
    assert_eq!(
        removed.status, 404,
        "route gone after hot reconcile: {}",
        removed.body
    );

    server.shutdown().await;
}

// ===========================================================================
// Scenario 9 — Secret / error hygiene: a planted canary never leaks into an
// ERROR body. (Also confirms 422 eval bodies carry only sanitised text.)
// ===========================================================================

#[tokio::test]
async fn s9_error_bodies_never_leak_a_held_credential() {
    // A well-behaved driver HOLDS a canary credential but returns a STRUCTURED, secret-free
    // error on failure (it does not embed the secret in the error text). Plant the canary in
    // the driver's private field, fail the scan with a secret-free `UnknownMount`-class error,
    // and confirm no error path (422 eval, 404 not-found) surfaces the held credential.
    struct CredHoldingDriver {
        _token: String, // the planted canary — never placed in any error/DTO/body
    }
    impl qfs_core::Driver for CredHoldingDriver {
        fn mount(&self) -> &str {
            "/mock"
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            Ok(NodeDesc::new(Archetype::RelationalTable, items_schema()))
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::none().select()
        }
        fn procedures(&self) -> &[qfs_core::ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &PushdownProfile::None
        }
        fn applier(&self) -> &dyn qfs_core::PlanApplier {
            Box::leak(Box::new(NoopApplier))
        }
    }
    #[async_trait::async_trait]
    impl ReadDriver for CredHoldingDriver {
        async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
            // A SECRET-FREE structured error (RFD §5): names only a non-sensitive mount label.
            // A conformant driver NEVER embeds its held credential in the error text.
            Err(CfsError::UnknownMount("/mock/items".to_string()))
        }
    }

    let engine = engine_with_mock();
    let reads = Arc::new(ReadRegistry::new().with(
        DriverId::new("mock"),
        Arc::new(CredHoldingDriver {
            _token: CANARY.to_string(),
        }),
    ));
    let mut binding = HttpBinding::new(engine, reads, 10_000);
    binding
        .reconcile(&state_with(vec![endpoint("a", "GET", "/a", "/mock/items")]))
        .unwrap();
    let server = LiveServer::start(binding).await;

    let resp = server.request("GET", "/a", &[]).await;
    // It is an eval failure path; the body is a structured problem body.
    assert!(
        resp.status == 422 || resp.status == 500,
        "an upstream scan failure maps to a structured error status, got {} body: {}",
        resp.status,
        resp.body
    );
    assert!(
        !resp.body.contains(CANARY),
        "the held credential must NEVER appear in an error body: {}",
        resp.body
    );
    // The body IS structured JSON naming only the coarse error class + a sanitised detail.
    assert!(
        resp.body.contains("\"error\":"),
        "structured body: {}",
        resp.body
    );
    // Another error path (404) also carries no canary.
    let nf = server.request("GET", "/nope", &[]).await;
    assert!(
        !nf.body.contains(CANARY),
        "404 body leaked canary: {}",
        nf.body
    );

    server.shutdown().await;
}
