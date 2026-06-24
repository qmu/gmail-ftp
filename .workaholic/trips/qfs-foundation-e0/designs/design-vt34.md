# Design vt34 — watchtower (event bus, webhooks, watchers, trigger dispatch) + qfs-crypto-core

Author: Constructor
Status: under-review
Reviewed-by:

## Content

### Step 0 (done first) — qfs-crypto-core pure leaf
SHA-256 was vendored three times (objstore SigV4, slack HMAC+ct_eq, cron run-id). Extracted a
**true pure leaf** `qfs-crypto-core` (ZERO deps, std-only) housing `sha256/hmac_sha256/hex_lower/
sha256_hex/constant_time_eq`, pinned to FIPS 180-4 + RFC 4231 vectors. Re-pointed all three crates
(objstore + cron via direct path, slack via `pub use qfs_crypto_core as hmac` to preserve its public
`qfs_driver_slack::hmac::*` surface). Added a STRICTER guard than http-core's:
`crypto_core_is_a_pure_leaf_single_sourcing_the_three_vendored_copies` asserts the dep set is EMPTY
and the three former copy-holders all depend on the leaf. Net-neutral test delta.

### Step 1 — topology
New leaf crate **`qfs-watchtower`**, consumed ONLY by the `qfs` binary (the serve composition root).
It depends on qfs-server (Binding/ServerState/TriggerDef/WebhookDef), qfs-exec (read path for watcher
polling + build_plan for the handler), qfs-core (Plan/StatementSpec/PlanSpec/Value), qfs-parser (AST
+ predicate Expr), qfs-pushdown (lower_predicate Expr->Predicate), qfs-types (Predicate/Row/Value),
qfs-secrets (signing-secret-by-handle), qfs-crypto-core (HMAC verify + constant_time_eq), tokio
(LocalBus MPSC + watcher tasks). It does NOT depend on qfs-runtime (the commit path is an INJECTED
`Committer`, exactly the qfs-cron pattern). Default-on `native` feature gates qfs-exec/qfs-server/
tokio; the PURE core (event/dispatch DTOs + predicate eval + dedup) builds on wasm32.

**Webhook HTTP serving decision (option b, binary-composes):** qfs-watchtower does NOT depend on
qfs-http and does NOT serve HTTP. Its `WebhookBinding` exposes a **pure async ingest handler** over
owned request data (`method, path, headers, body`) -> `IngestOutcome{status, published}`. The BINARY
(terminal sink) composes both routers: qfs-http's `serve_config_with` gains an optional fallback
handler (a `dyn Fn(&HttpRequest)->Option<Future<HttpResponse>>`) the binary wires to the watchtower
ingest for `/hooks/...` paths. qfs-http gains NO qfs-watchtower dep and vice-versa (they cross only
through owned DTOs + a closure at the binary). Guards: extend the CO-t29-4 exec-consumer allowlist to
admit qfs-watchtower (it is a leaf integration consumer of qfs-exec, the role qfs-cmd/qfs-http/qfs-cron
play); add `watchtower_binding_is_a_leaf_serve_consumer` (only `qfs` may consume it; not on
qfs-runtime). All existing guards stay green.

### TRIGGER WHERE (CO-t31-4) — wired into the grammar (additive, no new keyword)
`WHERE` is already frozen. Add `where_pred: Option<Box<Statement>>` to `ServerDdl`; parse an optional
`WHERE <expr>` clause in `server_ddl` (the predicate wrapped as `Statement::Query` over an empty
`VALUES` source + a single `PipeOp::Where(expr)` so it round-trips through StatementSpec serde with no
new AST). Thread it through `from_server_ddl` into `TriggerDecl.predicate`. Lower it into a new
`TriggerDef.predicate: StatementSource` (canonical StatementSpec). At dispatch, rehydrate via
`StatementSpec::from_canonical`, extract the `Where` Expr, lower via `qfs_pushdown::lower_predicate`,
and evaluate over `NEW.*` (a tiny pure evaluator over `qfs_types::Predicate` + a 1-row Schema built
from NEW). Failing predicate -> zero plans, zero driver calls, zero audit.

### Modules
- `event.rs`: owned `Event{id,source,kind,dedup_key,new:Row,received_at}` + `EventKind`. `dedup_key`
  = `source + ":" + native id/etag/@version`. Pure, wasm-portable.
- `bus.rs`: `EventBus` trait (publish/subscribe/ack) + `LocalBus` (tokio MPSC, bounded; a durable
  in-memory spool of un-acked events keyed by EventId; redelivery via `redeliver_unacked`). native-gated.
- `watcher.rs`: `Watcher{source,interval,cursor}` + `WatcherStore` trait + `WatcherCursor`; `poll_once`
  runs the source query through qfs-exec read, diffs vs cursor, emits events, persists cursor ONLY
  after publish.
- `webhook.rs`: `WebhookBinding` impl `Binding`; `reconcile` rebuilds the `/hooks/...` route set;
  `ingest` resolves the signing secret BY HANDLE from qfs-secrets, verifies HMAC-SHA256 via
  qfs-crypto-core + `constant_time_eq`, enqueues durably, returns 2xx; bad signature -> 401, enqueues 0.
- `dispatch.rs`: `Dispatcher::handle` — match Event vs TriggerDefs, eval WHERE over NEW.*, bind NEW.*
  into the handler plan (the qfs-http `bind_params` shape), lower via `build_plan`, call the policy
  gate HOOK (a trait, not implemented), PREVIEW-log -> COMMIT via injected Committer -> ack; on error
  bounded retry+backoff then dead-letter.
- `WatchtowerBinding`: top-level `Binding` owning bus + webhook routes + watcher set; `reconcile`
  converges all three (idempotent re-reconcile no-op; read snapshot, never hold RwLock across await).

### At-least-once + dedup
ack ONLY after successful COMMIT. LocalBus keeps un-acked events in a durable spool; a simulated crash
(drop the subscriber without ack) leaves them redeliverable. `dedup_key` carried end-to-end; the
dispatcher keeps a `seen: HashSet<dedup_key>` (the idempotency ledger) so a redelivered Event with the
same dedup_key is a no-op AFTER its first successful commit. The golden uses an UPSERT handler + a
counting fake committer: two deliveries -> ONE net effect. Documented: non-idempotent procs
(`CALL mail.send`) need an explicit dedupe guard in the plan.

### QA
native build/clippy/fmt; wasm32 pure-core build (then delete artifacts); workspace tests (was 1038);
the acceptance tests (plan assertion, idempotency, recovery, WHERE gating, webhook signed/bad, reconcile
add/remove + idempotent, one audit per fire, purity); ship fixtures/watchtower.qfs; all guards green.

## Review Notes
