# Design vt23 — Cloudflare D1 + KV + Queues driver

Author: Constructor
Status: approved (single-ticket coding-phase design)
Reviewed-by: (coding-phase implementation design; reviewed via Architect analytical review + Planner E2E)

## Content

### Scope
New leaf crate `crates/driver-cf` (`qfs-driver-cf`), mounted at `/cf`, exposing three
Cloudflare primitives through the uniform qfs DSL with their correct archetypes (RFD §5):

- **D1** `/cf/d1/<db>/<table>` — RelationalTable. **Reuses t17 `qfs-driver-sql`**: the
  `Dialect::Sqlite` emitter (`render_select`/`render_dml`), the `compile` query→SelectPlan
  lowering, and the `catalog`/`QuerySpec`/`Param`/`DmlOp` DTOs. D1 is SQLite-over-HTTP; the
  only new piece is a `D1Backend` HTTP seam that ships the rendered `(sql, params)` to the
  Cloudflare D1 REST API with `params` as a **structured bound array** (never interpolated —
  the injection-safety obligation the t17 Architect flagged for an HTTP backend). The D1
  **batch** endpoint is one atomic transaction: `commit_transaction(&[DmlOp])` → one `/batch`.
- **KV** `/cf/kv/<namespace>/<key>` — BlobNamespace. `ls/cp/mv/rm` + a degenerate
  `(key, value)` table for `SELECT`/`UPSERT`. TTL + metadata carried per entry.
- **Queues** `/cf/queue/<name>` — AppendLog. `INSERT` appends a message (`queue_send`),
  `SELECT … LIMIT n` tails (`queue_pull`); no WHERE/order/offset pushdown.

### Confined HTTP seam (the leaf invariant)
`trait CfBackend` is the single transport seam, owned DTOs in/out (no vendor type past the
boundary). The real impl `HttpApiBackend` rides `qfs_driver_http::HttpClient` (reqwest stays
confined in `qfs-driver-http`) — the same `Arc<dyn HttpClient>` seam google-auth adapts. The
Cloudflare API token is a `qfs_secrets::Secret` written into the `Authorization: Bearer`
header (redacted by the shared `qfs-http-core` redaction authority); never logged, never in a
`CfError`. The wasm `WorkersBindingBackend` is **named-parked** (no live wasm CI lane yet); the
DTOs + the `CfBackend` seam are wasm-clean so the binding impl drops in later behind the same
trait. A `MockCfBackend` answers from in-memory fixtures and records every call for tests.

### Crate dependency edges (G6 acyclic, runtime-leaf)
`qfs-driver-cf → { qfs-driver, qfs-plan, qfs-types, qfs-runtime, qfs-driver-sql,
qfs-driver-http, qfs-http-core, qfs-secrets, serde_json, thiserror, tracing }`. It is a
**runtime leaf** (bridges its synchronous `PlanApplier` → async `ApplyDriver`); nothing depends
back onto it, so tokio dead-ends. Append `qfs-driver-cf` to the `dep_direction.rs`
runtime-consumer allowlist (one-line reviewable signal). Depending on `qfs-driver-sql` (itself a
runtime leaf) is fine: that is a sideways edge between two leaves, no cycle.

### Effect mapping
- D1 `Insert/Upsert/Update/Remove` → reuse t17 `lower`-style DML build over the catalogued
  table → `DmlOp` → `render_dml(Sqlite, op)` → `d1_query`/`d1_batch` with bound params.
- KV `Upsert` → `kv_put(ns, key, value, ttl?, metadata?)`; `Remove`/`rm` → `kv_delete`;
  `cp`/`mv` → get→put(→delete) (mv = copy→verify→delete, RFD §6).
- Queue `Insert` → `queue_send(q, body, idempotency_key)` (the key makes at-least-once retry
  safe — no double-append).

### Capability gating (parse-time, structured)
Per-node `capabilities(path)`: a D1 table = full CRUD; a KV namespace = `{ls,select,upsert,
remove,cp,mv,rm}`; a queue = `{insert,select}` only — so `UPDATE /cf/queue/q` and `JOIN`/write
verbs over KV are rejected at the parse gate with `CfsError::UnsupportedVerb` (structured).

### Tests (mocked Cloudflare API + in-memory secrets — NO live network)
1. D1 SELECT with WHERE pushed to the SQLite-rendered SQL; assert the request body carries
   `params` as a **bound JSON array**, the SQL text contains only `?` placeholders, and a
   `'; DROP TABLE t; --` literal lands in the params array (NOT in the SQL) — injection-safe.
2. D1 INSERT/UPSERT/UPDATE/REMOVE → request shape + bound params.
3. D1 batch atomicity: a multi-op `commit_transaction` maps to ONE `/batch` request.
4. KV get/put/delete/list (+ TTL/metadata).
5. Queues send (idempotency key present) / pull (tail, capped by LIMIT).
6. Capability gating: `UPDATE` on a queue + a write verb on a KV namespace rejected structurally.
7. Token never leaks: planted-canary across every `CfError` Debug/Display + the redacting
   `HttpRequest` Debug carrying the `Authorization` header.
8. End-to-end through the interpreter + `PlanApplierBridge` for a D1 write, a KV upsert, and a
   queue send (`#[tokio::test]`, mock backend).

### Verification gates
`cargo fmt --all --check`; `cargo clippy --workspace --all-targets --all-features -D warnings`;
`cargo build --workspace`; `cargo test --workspace` — all green, no regression on 538, plus
qfs-plan purity + generic runtime-leaf confinement still green.

## Review Notes
Concern (engineering) — RESOLVED during implementation: a direct `qfs-driver-cf → qfs-driver-sql`
edge **fails** the `dep_direction` generic runtime-leaf confinement test, because `qfs-driver-sql`
is itself a runtime consumer and the invariant forbids any crate depending back onto a runtime
consumer (tokio must dead-end in each leaf). Fix applied: **extracted the pure SQL compile/emit
core into a new pure-leaf crate `qfs-sql-core`** (`Dialect`/`emit`/`compile`/`catalog`/`SqlError`,
deps = `qfs-types` only) — the same single-source pattern as `qfs-http-core`. Both `qfs-driver-sql`
and `qfs-driver-cf` now reuse the **one** sqlite emitter from `qfs-sql-core` while each stays an
**independent** runtime leaf. Likewise the HTTP path uses a **local `HttpExchange` seam over
`qfs-http-core`** (the `qfs-google-auth` precedent) instead of depending on `qfs-driver-http`. The
orphan-rule `From<SqlError> for EffectError` / `From<SecretError> for SqlError` impls moved to
explicit converters in `qfs-driver-sql` (both types now foreign to it). All gates green; the
`runtime_is_confined_to_plan_and_types` confinement test passes.
