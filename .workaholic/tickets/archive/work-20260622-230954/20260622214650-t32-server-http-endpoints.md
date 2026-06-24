---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, UX]
effort:
commit_hash: cb60feb
category: Added
depends_on: [20260622214650-t31-server-binding-ddl.md]
---

# Server: HTTP endpoints (query → API)

## Overview
Delivers the HTTP serving binding for `CREATE ENDPOINT <method> <route> AS <query>`: turning
endpoint config rows in the `/server/endpoints` registry into live HTTP routes, PostgREST/
Hasura-style, but over the **federated** cfs model rather than a single database. Implements
RFD §8 ("Bindings = what causes a plan to run"; `ENDPOINT`→Worker/route) and the §6 runtime
principle that an HTTP request is simply *a cause that makes a plan run*. Each request binds
its path/query params into the endpoint's stored query, evaluates that query (the **pure**
query side of the language, RFD §3), and encodes the resulting rows via the codec registry
(json/csv) into an HTTP response. By policy default an endpoint query is **read-only**: it
serves the query face of cfs, not effects — write endpoints are gated and deferred. This is
the request-driven half of the watchtower server; cron `JOB` firing and inbound `WEBHOOK`/
`TRIGGER` ingestion are sibling bindings.

## Scope
In scope:
- An `HttpBinding` implementing the `Binding` trait (t30) that reconciles the `/server/endpoints`
  registry into an `axum` (native) `Router`, rebuilding routes on every committed `/server` mutation.
- Path-param binding: `:param` / `{param}` segments in the route bound into the stored query
  as named parameters; query-string and (read) body params bound the same way.
- Query evaluation through the existing planner/interpreter: lower the endpoint's `StatementId`
  to a `Plan`, `COMMIT` it (pure query plan → no effects), collect resulting rows.
- Response encoding via the **codec registry** (RFD §3): `ENCODE json` (default) and `ENCODE csv`,
  selected by `Accept` header / `?format=` — owned `Row`→bytes, no vendor types.
- Read-only-by-default policy gate: an endpoint whose query contains effect nodes is refused at
  registration unless its `POLICY` explicitly allows writes (enforcement seam only; full POLICY
  engine is t34).
- HTTP status/error mapping from structured engine errors (`thiserror`) to JSON problem bodies.
- `cfs serve` wiring so the HTTP listener starts under the `Runtime` supervisor.

Out of scope (deferred):
- Cron `JOB` scheduler + `LAST_RUN()` → sibling E7 ticket (binding sibling of t30).
- Inbound `WEBHOOK`/`TRIGGER` ingestion / watchtower pollers → sibling E7 ticket.
- `POLICY` enforcement engine (capability gating at fire time) → t34; here only the read-only
  default check + a hook.
- Auth/credential presentation for callers (API keys, JWT) → E5/E8; this ticket assumes the
  `Runtime`'s already-loaded credentials and a trusted bind address.
- Cloudflare Worker mapping of `ENDPOINT` (`wasm32`, native bindings) → E7 deployment ticket;
  this ticket targets the native `axum` daemon, keeping the binding abstraction Worker-portable.
- Pagination/`LIMIT`/streaming large result sets beyond a bounded buffer → follow-up.

## Key components
Module `cfs-server::http` (new; behind the `serve` binary feature, native `axum` dep here only):
- `binding.rs`
  - `struct HttpBinding { state: Arc<RwLock<ServerState>>, world: Arc<World>, listener: SocketAddr, handle: Option<ServerHandle> }`
  - `impl Binding for HttpBinding { fn kind(&self) -> BindingKind { BindingKind::Http } fn reconcile(&mut self, state: &ServerState) -> Result<()> }`
    — `reconcile` builds a fresh `Router` from `state.endpoints` and atomically swaps it (axum
    `Router` is cheap to rebuild; hot-swap via shared `arc-swap` or DO-style handle).
- `route.rs`
  - `fn build_router(endpoints: &Map<Name, EndpointDef>, world: Arc<World>) -> Router`
  - `fn compile_route(def: &EndpointDef) -> (Method, RoutePattern)` — translate `/x/:id` into an
    axum path and a list of declared param names (validated against the query's free params).
- `handler.rs`
  - `async fn dispatch(State(ctx): State<EndpointCtx>, req: Request) -> Response` — generic
    handler closed over one `EndpointDef`; binds params → `QueryArgs`, evaluates, encodes.
  - `struct EndpointCtx { stmt: StatementId, params: Vec<Name>, default_codec: CodecId, world: Arc<World> }`
- `params.rs`
  - `struct QueryArgs(Map<Name, Value>)` — owned scalar values; `fn bind(pattern, path, query, body) -> Result<QueryArgs, BindError>`
    rejects missing/extra/untyped params with a structured `BindError`.
- `encode.rs`
  - `fn negotiate(headers: &HeaderMap, q: &Query) -> CodecId` (json default, csv on request)
  - `fn encode_rows(rows: RowStream, codec: CodecId, world: &World) -> Result<(Bytes, ContentType)>`
    — delegates to the **codec registry** (`Codec` trait, t-codec); no bespoke serializer.
- `policy.rs`
  - `fn assert_read_only(plan: &Plan, policy: Option<&PolicyDef>) -> Result<(), PolicyError>` —
    walks the lowered plan for `EffectKind` write nodes; default-deny writes (RFD §3 purity,
    §10 least-privilege). Real enforcement engine is t34; this is the registration-time gate.
- `error.rs` — `enum HttpError { Bind(BindError), Policy(PolicyError), Eval(EngineError), Encode(CodecError) }`
  with `IntoResponse` → status + JSON problem body (`{ "error", "detail", "param"? }`).
- `EndpointDef` reuse from t30 `state.rs` (`{ method, route, query: StatementId, codec, policy }`);
  add `codec: Option<CodecId>` and `policy: Option<Name>` fields if not already present (coordinate w/ t31 DDL).

## Implementation steps
1. Add the `serve`-feature `axum`/`tower-http` deps to `cfs-server` (native only; keep core
   crates dep-free so the Worker target stays clean).
2. Extend `EndpointDef` (or confirm with t31) to carry optional `codec` and `policy` handles.
3. `params.rs`: implement `RoutePattern` parsing (`:p` / `{p}`) and `QueryArgs::bind` with a
   structured `BindError` (missing/extra/type-mismatch), validating against the query's free vars.
4. `policy.rs`: implement `assert_read_only` by scanning the lowered `Plan` for write
   `EffectKind`s; default-deny, allow only when a `PolicyDef` grants it. Call this at route
   *compile* time (registration) and again at request time (defense in depth).
5. `handler.rs`: implement `dispatch` — bind args, fetch the `StatementId` plan from the
   registry, `COMMIT` the pure query plan against `world` with the bound args, stream rows.
6. `encode.rs`: content negotiation + delegate to the codec registry; bounded in-memory buffer
   with a configurable max-rows guard returning `413` when exceeded.
7. `route.rs` / `build_router`: assemble one axum route per endpoint; attach per-route timeout,
   request-id, and tracing layers (`tower-http`).
8. `binding.rs`: implement `HttpBinding::reconcile` to rebuild + hot-swap the router on every
   committed `/server/endpoints` change; bind the listener once in `Runtime::run`.
9. `error.rs`: map every structured engine error to an HTTP status (400 bind, 403 policy,
   404 unknown route, 422 eval, 500 internal) with a machine-readable JSON body.
10. Golden tests with an in-memory mock driver: register endpoints, drive requests via
    `tower::ServiceExt::oneshot`, assert status + encoded body — no live network/creds.

## Considerations
- **Least-privilege & secrets (RFD §10)**: endpoints are read-only by default; the write path
  is reachable only through an explicit `POLICY`. The handler never surfaces credentials or raw
  upstream errors to callers (sanitize `EngineError` → problem body); no token ever logged.
- **Idempotency/recovery (RFD §6)**: GET/read endpoints are naturally idempotent; should a write
  endpoint be policy-allowed later, require `UPSERT`/`@version`-ETag semantics from the query so
  retried/at-least-once requests converge — leave a documented seam, do not enable writes here.
- **Observability**: `tracing` span per request (route, bound params *names only*, status, row
  count, per-leg latency); reuse the audit ledger for any fired effect plan. Emit request-id.
- **Hard part — param/query binding without injection**: params bind as **typed values** into
  the pre-parsed query AST/plan, never string-spliced into DSL text. Resolve by threading
  `QueryArgs` into the planner as bound parameters (the query is compiled once at registration;
  requests only supply values), guaranteeing no parse-time injection surface.
- **Hard part — hot router swap under concurrency**: `reconcile` runs after a `/server` write
  while requests are in flight; rebuild the `Router` from a cloned registry snapshot and swap an
  `arc-swap` pointer — never hold the `RwLock` write guard across `.await` (mirrors t30 rule).
- **Hard part — federated query latency**: an endpoint query may fan out across sources; apply
  the engine's per-leg timeouts/circuit breakers (RFD §6) and a request deadline so one slow
  upstream cannot pin a Worker/handler.
- **Worker portability**: keep `HttpBinding` behind the generic `Binding` trait so the same
  `EndpointDef`→query→codec pipeline maps to a Cloudflare Worker `fetch` handler later (E7);
  isolate all `axum` types inside `cfs-server::http`.
- **Directory/coding standards**: owned DTOs and `Row`/`Value` types only — no axum/serde_json
  vendor types leak past `http::`; small consumer-side traits (`Codec`, `Driver`, `Binding`);
  `thiserror` structured error enums; codecs via the registry, not ad-hoc serializers.

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green.
- A `CREATE ENDPOINT GET /items/:id AS (FROM /mock/items |> WHERE id = :id)` registers a live
  route; a `oneshot` GET against a mock driver returns `200` with a JSON body of the matching
  rows — no live network/credentials.
- **Plan assertion**: an endpoint whose query lowers to a `Plan` containing any write
  `EffectKind` is **refused at registration** (and at request time) with a structured policy
  error unless a `PolicyDef` allows it — golden test on the plan/decision, not on execution.
- Content negotiation: the same endpoint returns `application/json` by default and `text/csv`
  (via the codec registry) under `Accept: text/csv` / `?format=csv`; bodies are golden-tested.
- Param binding errors (missing/extra/type-mismatch path or query param) return `400` with a
  machine-readable JSON problem body naming the offending param — no panic.
- Unknown route → `404`; query evaluation error → `422`; both as structured JSON.
- **Hot-reload**: committing a new `/server/endpoints` row makes the route serve on the next
  request without restart; removing it yields `404` — asserted via the `HttpBinding::reconcile`
  swap with a counting test double.
- Injection test: a path param containing DSL-like text (`'; REMOVE …`) is bound as a typed
  value and does **not** alter the query plan (golden plan identical to the benign case).
- A bounded result-size guard returns `413` (or paginates) rather than buffering unboundedly.
