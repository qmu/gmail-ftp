---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Config]
effort:
commit_hash: f068a38
category: Added
depends_on: [20260622214650-t30-server-runtime-and-self-config-driver.md]
---

# Deployment: EC2 daemon + Cloudflare Workers (wasm)

## Overview

This ticket delivers the two production deployment targets for the single `cfs`
binary described in RFD 0001 §8–§9: a long-lived **Linux/EC2 daemon** and a
**`wasm32` Cloudflare Workers** build. It implements the deployment mapping in
RFD §8: `ENDPOINT`→Worker `fetch`, `JOB`→Cron Triggers, `WEBHOOK`/event bus→
Queues, stateful watcher/`LAST_RUN`→Durable Object, and `/d1`/`/r2`/`/kv`→native
Workers bindings (not HTTP). It does **not** invent new runtime semantics: the
server-as-driver (`/server/...`) and the binding model are owned by t30; this
ticket is the *host adapter* layer that takes the already-built effect-plan
interpreter and server registry and runs them on each target. The point is that
"the runtime is just what causes a plan to run" (RFD §2) — here we provide two
such causes (a daemon event loop, and the Workers event handlers) plus the build
profiles and release artifacts that ship them.

## Scope

In scope:
- A `cfs-host` boundary: a `RuntimeHost` trait abstracting the platform
  (timers, inbound HTTP, queue consume/produce, durable state, native storage
  bindings) so the server core is host-agnostic.
- `host-daemon` impl: tokio-based EC2/Linux daemon (`cfs serve <config.cfs>`),
  systemd unit, graceful shutdown, signal handling, on-disk audit ledger.
- `host-workers` impl: `wasm32-unknown-unknown` entrypoints via the `worker`
  crate — `fetch`, `scheduled` (Cron), `queue`, and a Durable Object class —
  mapping each server binding archetype to its CF primitive.
- Native binding bridges so `/d1`, `/r2`, `/kv` drivers use Workers bindings on
  wasm and fall back to their HTTP clients on the daemon.
- Cargo build profiles (`[profile.release]`, wasm size profile, feature flags
  `host-daemon`/`host-workers`) and release artifacts (static musl binary,
  `.wasm` + generated `wrangler.toml` template, checksums).

Out of scope (deferred):
- Server binding semantics, `/server/...` self-config driver, `LAST_RUN()` —
  **t30** (dependency).
- `POLICY`/least-privilege enforcement logic and audit-ledger *schema* — defined
  by the security cross-cutting ticket (E8); here we only persist/emit it.
- Credential store encryption/retrieval internals — E5 (`/auth`); we consume the
  store trait, we do not implement it.
- `/d1`, `/r2`, `/kv` driver *grammar* and DTOs — E4; we only wire their native
  binding backend.

## Key components

New crate `cfs-host` (consumer-side traits, owned DTOs only — no `worker::*`,
`tokio::*`, or AWS types leak past this boundary):

```rust
/// What causes a plan to run, abstracted over EC2 vs Workers.
pub trait RuntimeHost {
    fn now(&self) -> Timestamp;
    async fn serve_endpoints(&self, router: EndpointRouter) -> Result<()>;
    async fn schedule_jobs(&self, jobs: &[JobBinding]) -> Result<()>;
    async fn consume_events(&self, sink: EventSink) -> Result<()>;
    fn durable(&self) -> &dyn DurableStore;     // watcher cursors, LAST_RUN
    fn native_store(&self, mount: &Mount) -> Option<NativeStoreHandle>; // d1/r2/kv
}

pub trait DurableStore {        // owned KV-ish DTO over DO storage / disk
    async fn get(&self, key: &StateKey) -> Result<Option<StateBytes>>;
    async fn put(&self, key: &StateKey, val: StateBytes) -> Result<()>;
    async fn cas(&self, key: &StateKey, expect: Option<StateBytes>, val: StateBytes) -> Result<bool>;
}
```

- `cfs-host-daemon` (feature `host-daemon`): `TokioHost: RuntimeHost`; axum/hyper
  listener for `ENDPOINT`; `tokio::time` interval driver for `JOB`; in-process
  mpsc + on-disk spool for the event bus; `FileDurableStore` (fsync'd) for
  watcher cursors/`LAST_RUN`; native stores = the drivers' HTTP clients.
- `cfs-host-workers` (feature `host-workers`, `crate-type = ["cdylib"]`):
  `WorkersHost: RuntimeHost`; `#[event(fetch)]`→`EndpointRouter`,
  `#[event(scheduled)]`→matched `JobBinding`, `#[event(queue)]`→`EventSink`;
  `#[durable_object] struct WatchtowerState` implementing `DurableStore`;
  `native_store` returns handles backed by `env.d1()/.bucket()/.kv()`.
- `EndpointRouter`, `JobBinding`, `EventSink`, `Mount`, `NativeStoreHandle`:
  owned DTOs produced by the server registry (t30), consumed identically by both
  hosts — the closed-core grammar and three registries are untouched; deployment
  adds **zero keywords**.
- The effects-as-data interpreter (`COMMIT : Plan -> World`) is the *only* impure
  call either host makes; hosts never bypass it, preserving the purity invariant.

## Implementation steps

1. Define `cfs-host` traits + owned DTOs; make the server core (t30) depend on
   `RuntimeHost` instead of any concrete runtime.
2. Add workspace feature flags `host-daemon` / `host-workers` (mutually
   exclusive in a build) and gate platform crates behind them.
3. Implement `cfs-host-daemon`: listener, interval scheduler, event spool,
   `FileDurableStore`, graceful shutdown (SIGTERM drains in-flight plans).
4. Ship a systemd unit + `cfs serve` wiring; persist the audit ledger to disk;
   wire native stores to driver HTTP clients.
5. Implement `cfs-host-workers`: the four `#[event]` entrypoints + the Durable
   Object; map `LAST_RUN`/watcher cursors onto DO storage via `DurableStore`.
6. Bridge `/d1`/`/r2`/`/kv` to native bindings on wasm; behind the same driver
   backend trait used by the daemon's HTTP path (capability set is identical).
7. Add build profiles: static `x86_64/aarch64-unknown-linux-musl` release;
   `wasm32` size-optimized profile (`opt-level="z"`, `lto`, `strip`,
   `panic="abort"`); generate a `wrangler.toml` template enumerating Cron,
   Queue, DO, and d1/r2/kv bindings from the parsed `config.cfs`.
8. CI: build both targets, run clippy, produce + checksum artifacts.

## Considerations

- **Least-privilege & secrets**: the daemon reads credentials from the E5 store
  (never env-dumped, never logged, RFD §10); on Workers, secrets come from
  `env` secret bindings and native `d1/r2/kv` bindings — wasm must never embed a
  token. The generated `wrangler.toml` references binding *names* only.
- **Idempotency / recovery** (RFD §6): Cron and Queue deliver **at-least-once**;
  job/event handlers must be replay-safe — drive them through `UPSERT` and
  `LAST_RUN()`/cursor `cas()` so a redelivery is a no-op. `cp`-style multi-step
  effects keep the copy→verify→delete ordering and reconstruct from the audit
  ledger after partial failure.
- **Hard part — single source over two runtimes**: tokio and wasm have disjoint
  async + I/O. Resolve by keeping *all* effect logic above `RuntimeHost`; no
  `tokio::` or `worker::` symbol appears in core. Watch for wasm gotchas: no
  threads, no `SystemClock` (`now()` comes from the host), 128 MB / CPU-time
  limits (push down and batch per RFD §6 so a Worker invocation stays bounded),
  and DO single-threaded concurrency for `LAST_RUN`.
- **Observability**: structured logs + audit ledger on both — stdout/journald on
  EC2, `console`/Tail Workers on CF; every fired plan is recorded (RFD §6, §8).
- **Directory/standards**: one crate per host, `cfs-host` is the only seam; DTOs
  owned; feature-gate, do not `cfg`-spray; thin clients, no heavy SDKs (RFD §9).

## Acceptance criteria

- `cargo build --release --features host-daemon` and
  `cargo build --target wasm32-unknown-unknown --features host-workers` both
  succeed; `cargo clippy --all-targets -- -D warnings` is clean for each feature.
- No `worker`, `tokio`, `hyper`, or vendor storage type is reachable from the
  server core crate (enforced by a dependency/`deny`-style test).
- **Plan assertion / golden tests** (no live creds): given a fixture
  `config.cfs` with one `ENDPOINT`, one `JOB EVERY`, one `WEBHOOK`, one watcher,
  and `/d1` + `/r2` + `/kv` references, a host-agnostic test asserts the produced
  binding set; a golden test asserts the generated `wrangler.toml` matches a
  checked-in fixture (Cron expr, Queue, DO class, and d1/r2/kv binding names).
- A `MockHost` test drives a `JOB` twice and a `WEBHOOK` event twice and asserts
  the committed effect set is identical (at-least-once idempotency).
- Daemon integration test: `cfs serve` boots, serves one `ENDPOINT` over loopback
  returning the expected `PREVIEW`/`-json` body, and shuts down cleanly on SIGTERM
  with the audit ledger flushed.
- Release job emits: musl binaries (both arches), `cfs.wasm`, `wrangler.toml`
  template, and a `SHA256SUMS` file.
