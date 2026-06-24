---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [DB, Infrastructure]
effort:
commit_hash: 7092e99
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t17-driver-sql-databases.md]
---

# Driver: Cloudflare D1 + KV + Queues

## Overview

Delivers a single Cloudflare driver crate exposing three of Cloudflare's primitives through cfs's
uniform DSL, each mapped to the correct archetype from RFD §5:

- **D1** (`/cf/d1/<db>/<table>`) — **relational/table** archetype. SQLite-compatible; reuses the
  SQL pushdown + dialect machinery from t17 (the SQL-databases driver).
- **KV** (`/cf/kv/<namespace>/<key>`) — **blob/namespace** archetype. `ls cp mv rm`; also exposed
  as a degenerate two-column (`key`, `value`) table for `SELECT`/`UPSERT`.
- **Queues** (`/cf/queue/<name>`) — **append/log** archetype. `INSERT` appends a message,
  `SELECT` tails (consumer pull / recent messages).

This implements RFD §5 (driver contract + four archetypes), §3 (closed-core grammar, zero new
keywords — everything plugs into the **paths** registry), and the Cloudflare deployment mapping in
RFD §8: when the binary runs as a `wasm32` Worker we use **native Workers bindings** (env bindings,
not HTTP); when it runs as the EC2/Linux daemon or CLI we use the **Cloudflare REST API**. The same
owned DTOs and capability surface are presented either way (RFD §9: vendor types never leak).

## Scope

In-scope:
- One `cf` driver crate registering three mounts (`/cf/d1`, `/cf/kv`, `/cf/queue`) with per-node
  archetype, schema, and capabilities for `DESCRIBE`.
- Dual transport: a `Backend` trait with two impls — `WorkersBinding` (wasm32, native bindings) and
  `HttpApi` (REST, all other targets) — selected at runtime by target/config.
- D1: delegate SQL parse/pushdown to the t17 shared SQL evaluator; D1-specific batch endpoint and
  result-shape adaptation. KV: blob verbs + key/value table view. Queues: append `INSERT`, tail
  `SELECT`.
- Capability gating: each node declares only the universal verbs it supports; unsupported verbs
  rejected at parse time with a structured error (RFD §5).
- Effects evaluate to Plan nodes only; no I/O until `COMMIT` (purity invariant, RFD §3).

Out-of-scope (deferred):
- **R2** object storage → its own driver ticket (E4); shares the blob archetype but distinct API.
- **Durable Objects** / `LAST_RUN` stateful watcher and **Cron Triggers / Queues-as-event-bus**
  server wiring → E7 Server tickets (this driver only exposes data nodes, not bindings).
- Credential store / secret material lifecycle → E5 Auth (this ticket only *consumes* a resolved
  `CfCredentials` handle).
- Generic REST driver and codec registry → separate E4/E3 tickets.

## Key components

New crate `crates/driver-cf` implementing the `Driver` trait from t13:

- `CfDriver` — top-level driver; `fn mounts() -> Vec<Mount>` registers `/cf/d1`, `/cf/kv`,
  `/cf/queue`; `fn describe(path) -> NodeSchema`; `fn capabilities(path) -> CapabilitySet`.
- `enum CfNode { D1Table { db, table }, D1Db { db }, KvKey { ns, key }, KvNamespace { ns },
  Queue { name } }` — parsed/owned path target; the **path is the type** (RFD §4).
- `trait CfBackend` — transport abstraction (owned DTOs in/out, no vendor types past boundary):
  - `async fn d1_query(&self, db, sql, params) -> Result<Rows>`
  - `async fn d1_batch(&self, db, stmts) -> Result<Vec<Rows>>`
  - `async fn kv_get/kv_put/kv_delete/kv_list(...)`
  - `async fn queue_send(&self, q, body: Bytes) -> Result<MsgId>`
  - `async fn queue_pull(&self, q, max) -> Result<Vec<QueueMsg>>`
  - impls: `WorkersBindingBackend` (cfg `target_arch = "wasm32"`, uses `worker` crate env
    bindings) and `HttpApiBackend` (thin `reqwest`/HTTP client, REST API + bearer token).
- Owned DTOs: `Rows`, `Row`, `KvEntry`, `QueueMsg { id, body, attempts }`, `MsgId`, `CfCredentials`
  — all serde, no `worker::*` / SDK leakage.
- Planning: D1 writes → reuse t17 `SqlEffect` plan nodes (driver id = `cf.d1`). KV → `BlobEffect`
  (Put/Delete/Copy/Move). Queues → `AppendEffect { target, payload, idempotency_key }`.
- Procedures: none required (CRUD is universal here); no `CALL` surface beyond what archetypes give.

## Implementation steps

1. Scaffold `crates/driver-cf` with feature/cfg split for `wasm32` vs native transport; wire into
   the driver registry (t13).
2. Define `CfNode` path parser + `mounts()`/`describe()`/`capabilities()`; static schemas for the
   key/value table and queue message shape.
3. Define `CfBackend` trait + owned DTOs; stub both impls behind the trait.
4. Implement `HttpApiBackend` (REST): D1 `/query` + `/batch`, KV get/put/delete/list, Queues
   send/pull; bearer auth from `CfCredentials`; per-leg timeout + bounded retry + UPSERT idempotency.
5. Implement `WorkersBindingBackend` (wasm32) against `worker` env bindings; map results into the
   same DTOs.
6. D1 relational path: register with the t17 SQL evaluator so `WHERE/SELECT/JOIN` pushdown and the
   SQLite dialect are reused; adapt D1 result envelope → `Rows`.
7. KV: implement blob verbs (`ls/cp/mv/rm` → list/copy/delete; `mv` = copy+verify+delete per RFD §6)
   and the degenerate key/value table (`SELECT`, `UPSERT INTO`).
8. Queues: `INSERT INTO /cf/queue/<name>` → `queue_send`; `SELECT … LIMIT n` → `queue_pull` tail.
9. Capability gating: reject e.g. `UPDATE` on a queue or `JOIN` on KV at parse time (structured
   error).
10. Tests: plan/golden assertions for all three nodes; mocked backend; transport-selection unit test.

## Considerations

- **Least-privilege & secrets (RFD §10):** `CfCredentials` (account id + scoped API token, or the
  binding handle) is injected, never logged; HTTP backend redacts auth headers. The REST path needs
  only the minimum token scopes (D1 read/write, KV read/write, Queues send/consume) — document them.
- **Dual transport is the hard part:** native Workers bindings and REST have different result
  envelopes, error shapes, and pagination. Resolve by making `CfBackend` the *only* seam and keeping
  both impls behind identical DTOs; a shared conformance test-suite runs against both (one mocked,
  one wasm-cfg-gated) so behavior can't drift.
- **Idempotency / recovery (RFD §6):** KV `mv` = copy→verify→delete; queue `INSERT` carries an
  `idempotency_key` so at-least-once retries don't double-append; D1 writes use `UPSERT` where the
  caller asks for retry-safety. Effects are Plan nodes (purity invariant) — nothing runs before
  `COMMIT`, so `PREVIEW` is a true dry-run.
- **Queues tail semantics:** Queues is a consumer-pull primitive, not a random-access table; `SELECT`
  is bounded-tail only (no `WHERE` pushdown, no offset) — capabilities must advertise exactly that so
  the AI gets a correct, structured rejection rather than a runtime surprise.
- **D1 limits:** statement/row caps and the batch endpoint; surface D1 errors as the engine's
  structured driver error, not a raw HTTP/SDK string.
- **Observability (RFD §6):** per-leg timeouts, bounded retries, circuit breaker on the HTTP backend;
  every applied effect lands in the audit ledger.
- **Directory/coding standards:** thin HTTP client only (no heavy `cloudflare` SDK — RFD §9 footprint);
  owned DTOs; consumer-side small `CfBackend` trait; `clippy -D warnings`; both targets must compile
  (`x86_64`/`aarch64` native and `wasm32-unknown-unknown`).

## Acceptance criteria

- `cargo build` succeeds for native **and** `wasm32-unknown-unknown` (HTTP backend behind native cfg,
  binding backend behind wasm cfg); `cargo clippy -D warnings` clean; `cargo test` green.
- `DESCRIBE /cf/d1/<db>/<table>`, `/cf/kv/<ns>`, `/cf/queue/<name>` each return the correct archetype,
  schema, and capability set.
- **Plan assertions (no live creds):** `INSERT INTO /cf/queue/q VALUES …` produces an `AppendEffect`
  with an idempotency key and does **no** I/O under `PREVIEW`; `UPSERT INTO /cf/kv/ns` produces a
  `BlobEffect`; `SELECT … FROM /cf/d1/db/t WHERE …` pushes the predicate into the D1 SQL leg.
- **Capability gating:** `UPDATE /cf/queue/q` and `JOIN` over `/cf/kv/...` are rejected at parse time
  with a structured (not panicking) error.
- **Backend conformance:** the same golden test-suite passes against the mocked HTTP backend and the
  wasm-binding backend (DTO outputs identical); transport selection chooses bindings on `wasm32` and
  REST otherwise.
- No vendor (`worker::*` / SDK) type appears in any public signature of `driver-cf`.
