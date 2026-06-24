---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash: fe385cd
category: Added
depends_on: [20260622214650-t31-server-binding-ddl.md]
---

# Server: event bus, inbound webhooks, source watchers (watchtower)

## Overview
Delivers the **watchtower** engine: the "cause" side of the server runtime that turns
external change into fired effect-plans. Implements RFD §8 ("This is *watchtower*: watch
services (webhooks + pollers) → run effect-plans") on top of the §6 runtime principle that
the runtime is simply *what causes a plan to run*. Two cause sources are built here:
**inbound webhooks** (`CREATE WEBHOOK <name> AT /hooks/...` — append/log archetype, RFD §5)
that receive external HTTP events, and **polling watchers** that periodically re-query a
source path and diff for change. Both emit normalized **events** onto an internal **event
bus**; registered `CREATE TRIGGER <name> ON <event> [WHERE …] DO <plan>` handlers match an
event, bind `NEW.*` fields from the event payload into the plan, and fire it through the
existing `COMMIT` path. Delivery is **at-least-once**, so handlers must be idempotent
(`UPSERT`, RFD §6) — the engine guarantees re-delivery on crash/retry, not exactly-once.
This maps to Cloudflare **Queues** (event bus) + **Durable Objects** (stateful watcher
cursor / `LAST_RUN`), RFD §8 deployment table.

## Scope
In scope:
- An internal **event bus**: `EventBus` with bounded MPSC channels + durable spool, carrying
  normalized `Event` DTOs; at-least-once delivery with ack/redelivery.
- **Inbound webhook ingestion**: an HTTP `Binding` (the t31 binding seam) that serves the
  routes declared by `WebhookDef` under `/hooks/...`, validates signatures, and publishes one
  `Event` per request.
- **Source watchers (pollers)**: a supervised set of watcher tasks, one per `TriggerDef` whose
  `on` is a poll source; each holds a cursor, re-runs the source query on an interval, diffs,
  and emits `Event`s for new/changed rows.
- **Trigger dispatch**: match an `Event` against registered triggers, evaluate the optional
  `WHERE` predicate over `NEW.*`, bind `NEW.*` into the handler plan, and `COMMIT` it.
- **At-least-once + idempotency plumbing**: per-event dedupe key, ack-on-success, bounded
  retry with backoff, dead-letter spill; audit ledger entry per fired plan (RFD §6, §10).
- Reconcile-from-registry: watchers/webhook routes are (re)derived from `ServerState` on every
  committed `/server` mutation via `Binding::reconcile` (t30/t31), no imperative add/remove.

Out of scope (deferred):
- The frozen `CREATE WEBHOOK|TRIGGER` DDL parsing → E1 (consumed here as AST/Plan).
- The `/server` registry + `Runtime` supervisor + `Binding` trait → t30.
- HTTP serving primitives (axum app, route table) shared with `ENDPOINT` → t31.
- Cron `JOB` scheduler + `LAST_RUN()` → sibling job ticket (watchtower reuses its interval util).
- `POLICY` enforcement / capability gating at fire time → sibling policy ticket (this ticket
  calls the gate hook but does not implement the engine).
- Cloudflare Queues/DO concrete bindings → E7 deployment-mapping ticket (this ticket defines
  the `EventBus`/`WatcherStore` traits those back).

## Key components
Module `qfs-server::watchtower` (no HTTP/cron deps beyond the t31 seam):
- `event.rs` — owned, vendor-free DTO (RFD §9):
  - `struct Event { id: EventId, source: SourcePath, kind: EventKind, dedup_key: String, new: Row, received_at: DateTime }`
  - `enum EventKind { Webhook, RowAppended, RowChanged, RowRemoved }`. `new: Row` exposes the
    `NEW.*` fields a handler binds. No raw vendor request type leaks past ingestion.
- `bus.rs` — `trait EventBus { async fn publish(&self, e: Event) -> Result<()>; async fn subscribe(&self) -> EventStream; async fn ack(&self, id: EventId) -> Result<()>; }`
  with `struct LocalBus` (tokio MPSC + sled/spool for durability) as the EC2 impl; the CF
  Queues impl lands in the deployment ticket behind this trait.
- `webhook.rs` — `struct WebhookBinding` implementing `Binding` (t31):
  `reconcile(&mut self, state: &ServerState)` rebuilds the `/hooks/...` route set from
  `WebhookDef`s; an axum handler `async fn ingest(...) -> StatusCode` verifies the per-webhook
  signing secret (resolved by handle from the credential store, t27 — never inlined) and calls
  `bus.publish(Event{ kind: Webhook, .. })`. Returns 2xx **after** durable enqueue (at-least-once).
- `watcher.rs` — `struct Watcher { source: SourcePath, interval: Duration, cursor: WatcherCursor }`
  and `trait WatcherStore { fn load(&self, k: &str) -> Option<WatcherCursor>; fn save(&self, k: &str, c: &WatcherCursor) -> Result<()>; }`
  (the DO-backed cursor seam). `async fn poll_once(&mut self, world: &World, bus: &dyn EventBus)`
  re-runs the source query (pure read), diffs against the cursor, emits `Event`s, persists cursor.
- `dispatch.rs` — `struct Dispatcher { state: Arc<RwLock<ServerState>>, audit: AuditSink }`
  with `async fn handle(&self, e: Event, world: &mut World) -> Result<()>`: select matching
  `TriggerDef`s, evaluate `WHERE` over `NEW.*` (reuse the pure predicate evaluator, E1), bind
  `NEW.*` → lower handler to `Plan` (t09) → call the policy gate hook → `COMMIT` → `ack`.
- `WatchtowerBinding` — top-level `Binding` owning the bus, webhook routes, and watcher task
  set; `reconcile` converges all three to `ServerState`.
- No new keywords; `WEBHOOK`/`TRIGGER`/`ON` are frozen core DDL, drivers add zero keywords (RFD §3).

## Implementation steps
1. Define the `Event`/`EventKind` DTOs (`event.rs`) with serde; derive a stable `dedup_key`
   (source + native id/etag/`@version`) for idempotent re-delivery.
2. Define the `EventBus` trait + `LocalBus` (tokio MPSC + durable spool for crash-replay);
   implement `publish`/`subscribe`/`ack` with bounded capacity and redelivery on un-acked.
3. Define `WatcherStore` + `WatcherCursor`; provide an in-process impl now (file/sled), DO impl
   deferred. Cursor carries last-seen marker per source.
4. Implement `Watcher::poll_once`: run the source query through the read path (pure), diff vs
   cursor, emit `RowAppended/Changed/Removed` events, persist the cursor only after publish.
5. Implement `WebhookBinding`: build axum routes from `WebhookDef`s under `/hooks/...`; verify
   signature via credential handle (t27); enqueue durably, then return 2xx (ack-after-enqueue).
6. Implement `Dispatcher::handle`: trigger match → `WHERE` over `NEW.*` → bind `NEW.*` → lower
   to `Plan` → policy gate hook → `PREVIEW`-log → `COMMIT` → `bus.ack`; on error, retry w/ backoff
   then dead-letter.
7. Implement `WatchtowerBinding::reconcile`: diff desired (`ServerState`) vs running webhook
   routes + watcher tasks; spawn/cancel watchers, swap the route table; idempotent convergence.
8. Wire `WatchtowerBinding` into the `Runtime` supervisor (t30) alongside the t31 HTTP binding;
   share the `Arc<RwLock<ServerState>>` snapshot.
9. Emit an audit ledger record per fired plan (event id, trigger, plan summary, outcome) (RFD §6).
10. Golden + replay tests (see acceptance); ship a `fixtures/watchtower.qfs` config.

## Considerations
- **At-least-once is the hard part.** Exactly-once is not promised; correctness comes from
  idempotent handlers (`UPSERT`/`@version`) plus a `dedup_key` the engine carries end-to-end.
  Resolve by: ack only after successful `COMMIT`; durable spool so a crash between publish and
  ack redelivers; document that triggers using non-idempotent procs (`CALL mail.send`) need an
  explicit dedupe guard in the plan. (RFD §6 idempotency, §10.)
- **Least-privilege & secrets**: webhook signing secrets and source credentials are resolved by
  handle from the encrypted store (t27), never inlined in `WebhookDef`/`TriggerDef`, never
  logged (RFD §10). Every fired plan passes through the policy gate hook so an unconstrained
  handler cannot run once the policy engine lands.
- **Recovery**: watcher cursor is persisted only after the corresponding events are durably
  published, so restart re-emits at most a bounded window (at-least-once), never silently skips.
  Dead-letter spill is inspectable via `/server` for operator replay.
- **Concurrency**: watcher tasks and the dispatcher share `Arc<RwLock<ServerState>>`; take a
  read snapshot, never hold the write guard across `.await`; clone the trigger set for dispatch.
  Webhook ingestion must not block on `COMMIT` — it enqueues and returns.
- **Observability**: `tracing` spans per event (ingest → dispatch → commit → ack), per poll
  cycle, and per redelivery; structured fire records in the audit ledger (RFD §6, §10);
  per-leg timeouts + bounded retries + dead-letter (circuit-breaker-friendly).
- **Directory/coding standards**: owned DTOs, small consumer-side traits (`EventBus`,
  `WatcherStore`) so EC2 (sled/tokio) vs CF (Queues/DO) swap without touching dispatch;
  `thiserror` structured errors; no vendor request types past ingestion (RFD §9).

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green.
- **Plan assertion**: an injected `Event` matching a `TriggerDef` produces the expected handler
  `Plan` with `NEW.*` correctly bound — asserted on the lowered plan (golden), no live `COMMIT`
  against a real service, no live credentials.
- **Idempotency / at-least-once**: delivering the same `Event` (same `dedup_key`) twice through
  `Dispatcher::handle` yields a single effective change — golden test with an `UPSERT` handler
  and a counting fake driver showing one net effect across two deliveries.
- **Recovery test**: a `LocalBus` un-acked event is redelivered after a simulated crash
  (drop without ack), and a watcher cursor restored from `WatcherStore` resumes without
  skipping the unpublished window.
- **WHERE gating**: an event failing the trigger's `WHERE NEW.*` predicate fires **no** plan
  (assert zero audit records, zero driver calls).
- **Webhook**: a signed request to a reconciled `/hooks/<name>` route enqueues exactly one
  `Event` and returns 2xx; an invalid-signature request enqueues none and returns 401 (tested
  with a fixture secret, no external network).
- **Reconcile**: adding/removing a `WebhookDef`/`TriggerDef` in `ServerState` and calling
  `reconcile` spawns/cancels exactly the right webhook routes and watcher tasks (idempotent
  re-reconcile is a no-op), asserted via test doubles.
- Every fired plan writes one audit ledger record (event id + trigger + outcome).
- Unit tests confirm purity: building the handler plan and evaluating `WHERE` perform no I/O
  and no state mutation until `COMMIT`.
