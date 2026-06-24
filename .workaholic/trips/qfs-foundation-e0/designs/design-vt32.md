# Design vt32

- Author: Constructor
- Status: implemented
- Reviewed-by: (pending Architect/Planner)

## Content

### Ticket
t32 — Server HTTP endpoints (query → API). Bind the `/server/endpoints` registry (t31)
into live HTTP routes through the `Binding` reconcile seam (t30), evaluating each endpoint's
pre-parsed query via the `qfs-exec` read executor (t29) and encoding rows via the codec
registry (t15).

### HEADLINE: binding-placement topology

Two existing guards (`crates/cmd/tests/dep_direction.rs`) constrain the placement:
- **CO-t29-4** (`exec_is_confined_above_the_spine_and_off_the_runtime`): only `{qfs-cmd, qfs}`
  may consume `qfs-exec`; no spine crate may reach up into it.
- **CO-t30-1** (`runtime_is_confined_to_plan_and_types` + the Architect ruling): `qfs-server`
  must NOT host an async binding executor; `Binding::reconcile` stays synchronous + owned
  snapshot; async binding executors live in **leaf binding crates** or the binary.

**Decision: a NEW leaf crate `qfs-http`** (the HTTP binding), NOT composition in `qfs-server`
and NOT inline in the binary. Rationale:
- `qfs-server` cannot host it (would need `qfs-server → qfs-exec`, tripping CO-t29-4; and the
  async listener would violate CO-t30-1's "server is runtime-free" ruling).
- The binary *could* host it (t28 precedent for the shell adapter), but the HTTP binding is a
  cohesive unit (router/handler/params/encode/policy/error ≈ 8 modules) — too much logic for
  the thin entrypoint. A dedicated leaf crate keeps the binary thin and gives the binding a
  testable home (oneshot tests live in the crate, no TCP).
- `qfs-http` depends on `qfs-server` (Binding/ServerState/EndpointDef), `qfs-exec`
  (execute_read/ReadDriver/ReadRegistry), `qfs-core` (CodecRegistry/MountRegistry/Value/the
  rehydrated StatementSpec via re-export), and tokio (the async listener + arc-swap router).
- It is a **leaf**: nothing depends on it except the terminal `qfs` binary (which wires
  `qfs serve`). tokio therefore dead-ends in the binary exactly as the t28 runtime-leaf
  exemption requires.

**Guards kept green / extended:**
- CO-t29-4: extend `allowed_exec_consumers` to `["qfs-cmd", "qfs", "qfs-http"]` with a
  documented rationale (qfs-http is a leaf integration consumer of the read executor, the
  same role qfs-cmd plays — it is NOT a spine/lower crate reaching up).
- CO-t30-1: `qfs-server` gains NO new deps; `Binding::reconcile` stays sync + owned snapshot;
  the async listener + router swap live entirely in `qfs-http`.
- Runtime-leaf confinement: `qfs-http` does NOT depend on `qfs-runtime` (it uses tokio
  directly for the listener but never the COMMIT interpreter), so the
  `runtime_consumers_allowed` list is untouched. It IS a tokio user, but the guard fires only
  on `qfs-runtime` consumers, and qfs-http is a leaf regardless.
- Binary allowlist (`binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root`):
  extend the allowed binary workspace deps with `qfs-http` (the serve composition root, the
  HTTP sibling of the t28 shell composition root).
- Terminal-sink invariant: nothing depends on `qfs-http` except the binary — asserted by a
  new guard clause so the leaf property is mechanically pinned.

### axum-vs-in-house footprint decision

`cargo add axum --dry-run` resolves axum 0.8.9 from the index, but the **`.crate` file is NOT
in the offline cache** (`~/.cargo/registry/cache` has hyper/http/tower/tower-http but no
`axum-*.crate`, no `matchit-*.crate`). The `--offline` dry-run passes only because the index
metadata exists; a real `cargo build` would download axum + axum-core + matchit + transitive
deps. Disk is at **98% (2.2G free)** — pulling an uncached dep tree is unsafe.

**Decision: in-house minimal request pipeline, NO axum.** The route table, param binding,
eval, and encode pipeline are implemented as plain owned Rust functions operating on owned
request/response DTOs (`HttpRequest { method, path, query, headers, body }` → `HttpResponse {
status, content_type, body }`). Tests drive the pipeline **in-process** (a direct
`Router::dispatch(&req)` call — the oneshot analogue) with NO TCP at all. The live `qfs serve`
path uses `tokio::net::TcpListener` + a tiny HTTP/1.1 line parser sufficient for the endpoint
contract (request line + headers + body → owned `HttpRequest`; owned `HttpResponse` → wire
bytes). This reuses the already-cached tokio (no new crate). ADR `docs/adr/0004-http-serving.md`
records the footprint/offline reasoning, consistent with ADR-0002/0003.

No vendor HTTP type leaks past the `qfs-http` crate boundary — the `Binding` trait stays
generic, so the same `EndpointDef → query → codec` pipeline maps to a CF Worker `fetch` later
(E7/t35): only the thin wire-parse/serialize shim is native-specific.

### Injection-safe typed param binding (the hard part)

The endpoint query is stored (t31) as a span-normalised `StatementSpec` canonical-JSON string
in the `/server/endpoints` row's `query` column. The grammar has **no `:param` placeholder**
(`:` is not lexable). The injection-safe convention, using the EXISTING grammar:

1. **Route declares params**: `:id` / `{id}` segments in the route string. At registration I
   parse the route into a `RoutePattern` (literal vs param segments) and extract the declared
   param **names**.
2. **Query references params as bare column refs**: the stored query references each param by
   the same name as a bare identifier (`FROM /mock/items |> WHERE id = id`). The convention:
   a free column reference whose name matches a declared route/query param is a **parameter
   slot**, not a row column.
3. **Bind = typed AST rewrite, parsed ONCE**: at request time I rehydrate the StatementSpec
   (`from_canonical`, NO re-parse), then walk the AST and replace every `Expr::Col(name)`
   whose `name` is a declared param with `Expr::Lit(literal)` where `literal` is the
   **typed** conversion of the request value (`Value::Int` → `Literal::Int`, `Value::Text` →
   `Literal::Str`, etc.). The request value NEVER re-enters the parser and is NEVER spliced
   into DSL text — it becomes a single typed literal AST node.

This guarantees zero parse-time injection surface: a path param `'; REMOVE /mail/inbox`
becomes `Expr::Lit(Literal::Str("'; REMOVE /mail/inbox"))` — one string-literal node,
producing a plan **structurally identical** to a benign string. The injection golden test
asserts the lowered plan (and the rewritten AST) is byte-identical between the malicious and
benign bind.

`QueryArgs` collects the typed `Value`s from path + query-string + (read) body. A
missing/extra/untyped param → structured `BindError` naming the offending param → HTTP 400.

### Read eval reuses the qfs-exec read executor

After the typed AST rewrite, the bound `Statement` is handed to
`qfs_exec::execute_read(&stmt, &mounts, &reads)` (or `block_on_read` from the sync reconcile
path's handler future). The mock `ReadDriver` (the t29 fake) is registered into the
`ReadRegistry`; the executor runs parse-already-done → resolve → plan → scan → residual →
`RowSet`. NO new eval logic in qfs-http.

### Read-only policy gate

`assert_read_only(plan, policy)` walks the lowered `Plan` for write `EffectKind`s
(`ServerConfigWrite`, `EffectKind::Write`/insert/update/remove effects). Default-deny: any
write effect → `PolicyError` UNLESS a `PolicyDef` grants it. Enforced at **registration**
(route compile — the plan-assertion acceptance: a write-lowering endpoint is REFUSED at
registration) AND at **request time** (defense in depth). Full POLICY engine is t34; this is
the gate + hook. A read query (`FROM … |> WHERE …`) lowers to a pure plan (no effect nodes) →
allowed.

### Encode + bounded result guard

`negotiate(headers, query)` → codec format: `json` default; `csv` on `Accept: text/csv` or
`?format=csv`. `encode_rows(rowset, fmt, codecs)` resolves the codec from the registry and
encodes the owned `RowBatch` → bytes + content-type. A configurable `max_rows` guard returns
`413` when the result exceeds the bound (checked before encode).

### Error mapping → JSON problem body

`HttpError` enum → `(status, JSON { error, detail, param? })`:
- 400 Bind (names the offending param), 403 Policy, 404 unknown route, 422 Eval, 500 Internal,
  413 Oversize. Engine errors are **sanitized** (the executor's `ExecError` already carries
  secret-free messages; qfs-http maps the kind + a sanitized detail, never raw upstream
  errors, never credentials). No token is ever logged.

### EndpointDef field coordination (t30/t31)

`qfs-server`'s runtime `EndpointDef` (state.rs) currently carries `{name, method, route,
query: StatementSource}`. I add `policy: Option<String>` (the t31 `policy_ref` seam landed in
the row) so the binding can read the policy handle. The `codec` is read from the query's
`ENCODE` clause / the request negotiation, so no `codec` column is strictly required, but I
add `codec: Option<String>` for the explicit default-codec override the ticket lists. The
`apply_server_write` insert path (driver.rs) is extended to read the `policy`/`codec` columns
(coordinated with the t31 `server_node_schema(Endpoints)` columns — I confirm/extend the
schema if the columns are absent).

### Modules (in `crates/http/`)
- `lib.rs` — crate root, re-exports, the owned `HttpRequest`/`HttpResponse` DTOs.
- `binding.rs` — `HttpBinding` impl `Binding` (kind `Http`); arc-swap router; reconcile from
  owned snapshot.
- `route.rs` — `RoutePattern` parse (`:p`/`{p}`), `build_router`, `compile_route` (+
  registration-time policy gate).
- `params.rs` — `QueryArgs`, typed bind from path/query/body, `BindError`.
- `rewrite.rs` — the typed AST param-substitution (the injection-safe core).
- `handler.rs` — `dispatch`: bind → rewrite → execute_read → encode → response.
- `encode.rs` — `negotiate` + `encode_rows` + the 413 guard.
- `policy.rs` — `assert_read_only`.
- `error.rs` — `HttpError` → status + JSON problem body.
- `serve.rs` — the `tokio::net::TcpListener` HTTP/1.1 loop (live path) + wire parse/serialize.

### Quality strategy
- Unit + oneshot tests (in-process dispatch, no TCP): register + GET 200 JSON; write-endpoint
  refused at registration; json/csv negotiation; bind error → 400 naming param; unknown route
  → 404; eval error → 422; hot-reload via reconcile swap with a counting double; injection
  (identical plan golden); 413 oversize.
- `cargo build` / `clippy -D warnings` / `fmt` green; keep all dep_direction guards green
  (extend the three allowlists coherently with documented rationale); 959 baseline not
  regressed.

## Review Notes
(pending Architect/Planner review)
