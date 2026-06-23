# ADR 0005 — Deployment hosts: the `RuntimeHost` seam, the daemon over the existing composition, and the parked CF Workers host

- **Status**: Accepted (locked)
- **Date**: 2026-06-23
- **Deciders**: cfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t36 — Deployment targets: EC2 daemon + Cloudflare Workers (wasm). The *host-adapter*
  layer that runs the already-built effect-plan interpreter + server registry on two targets.
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §1 (single, *lean* binary + `wasm32` Workers target), §2 ("the runtime
  is just what causes a plan to run"), §6 (at-least-once / idempotency / recovery), §8 (bindings =
  what causes a plan to run; `ENDPOINT`→fetch, `JOB`→Cron, `WEBHOOK`/event→Queue, watcher/`LAST_RUN`
  →Durable Object, `/d1`·`/r2`·`/kv`→native bindings), §9 (no heavy vendor SDKs; owned DTOs at the
  boundary), §10 (least-privilege; wasm must never embed a token). ADR-0001 / ADR-0002 / ADR-0003 /
  ADR-0004 (the same footprint / offline-cache / wasm-buildability decision shape — winnow over
  chumsky, in-house combine engine over DuckDB, in-house git reader over gix, in-house HTTP/1.1
  over axum).

## Decision

**t36 introduces ONE host-adapter seam — the `cfs-host` crate — that abstracts *what causes a plan
to run* over the two production targets, inventing NO new runtime semantics.** Three decisions:

1. **The `cfs-host` seam is a pure, wasm-clean leaf.** It carries the consumer-side traits
   (`RuntimeHost`: `now`/`serve_endpoints`/`schedule_jobs`/`consume_events`/`durable`/`native_store`;
   `DurableStore`: `get`/`put`/`cas`) and owned, vendor-free DTOs ONLY (`BindingSet` +
   `EndpointBinding`/`JobBinding`/`WebhookBinding`/`WatcherBinding`/`NativeStoreBinding`/`Mount`/
   `NativeStoreHandle`/`Timestamp`/`StateKey`/`StateBytes`). No `worker::*`, `tokio::*`, or vendor
   storage type ever crosses this seam. With default features it builds on
   `wasm32-unknown-unknown` (verified) and pulls no async runtime — the traits use native
   async-fn-in-trait (Rust ≥1.75) so the core needs no `async-trait` crate; `DurableStore` returns
   a boxed future so it stays object-safe behind `&dyn DurableStore`.

2. **The `host-daemon` (EC2/Linux) host REUSES the existing serve composition behind the trait — it
   does not rebuild it.** The HTTP listener (`cfs-http`), the cron interval (`cfs-cron`), and the
   watchtower bus + `/hooks/...` ingest (`cfs-watchtower`) are already wired in
   `crates/cfs/src/serve.rs` (t32/t33/t34). The daemon's `TokioHost: RuntimeHost`, composed in the
   terminal `cfs` binary, FORMALIZES those already-wired causes under the trait. The two NEW
   daemon-side primitives t36 adds are an fsync'd `FileDurableStore` (watcher cursors / `LAST_RUN`
   that survive a restart, written via write-temp→fsync→rename→dir-fsync so a crash never leaves a
   torn cursor) and an on-disk append-only `AuditLedger` (the persistent fired-plan record that
   replaces the in-memory drain for the long-lived daemon). Both write under a project-local state
   dir (`CFS_STATE_DIR`, default `.cfs-state`; the systemd `StateDirectory=/var/lib/cfs` in
   production) — never a system path.

3. **The `host-workers` (Cloudflare Workers) host is PARKED behind its feature.** The `worker`
   crate (the CF Workers Rust SDK) is **not in the offline cache** — it is not even resolvable from
   the offline crates.io index on the trip host (`cargo` reports "no matching package named
   `worker`" under `--offline`; the cache holds `wasm-bindgen`/`js-sys`/`web-sys` but no `worker`,
   `worker-sys`, or `wasm-streams`). Per the ADR-0002/0003/0004 footprint reasoning (an uncached
   heavy dependency tree on a ~99 %-full disk is the exact risk those ADRs were written to avoid),
   the real `#[event(fetch/scheduled/queue)]` + `#[durable_object]` entrypoints are PARKED. What
   ships is the wasm-clean SCAFFOLD: the binding-archetype → CF-primitive mapping (`endpoint_event`
   →`fetch`, `job_event`→`scheduled`/Cron, `webhook_event`→`queue`, `watcher_event`→the
   `WatchtowerState` Durable Object, native stores→`env.d1()/.bucket()/.kv()`) and the DTO wiring,
   so the entrypoints are a mechanical drop-in once `worker` lands. The `host-workers` feature
   builds on `wasm32-unknown-unknown` today (verified) and takes NO `worker` dependency.

The two host features (`host-daemon` / `host-workers`) are **mutually exclusive** in a single build
(a `compile_error!` fires if both are set — one deployment target per binary).

## Context

The ticket's preferred shape was the full `worker`-crate Workers host. Two facts shaped the scope,
each mirroring an earlier ADR:

- **Offline-cache miss (the `worker` crate, like `axum` at t32).** `worker` is uncached and
  unresolvable offline; building it would download `worker` + `worker-sys` + `wasm-bindgen` glue +
  a `wasm-streams`/`js-sys`/`web-sys` tree from the network onto a near-full disk. ADR-0004 made the
  identical call for `axum`; t36 makes it for `worker`. The reversibility seam is the same: the
  binding-archetype mapping is the one boundary, so the real entrypoints replace only the parked
  scaffold module when `worker` is cached.

- **musl static cross-link is CI-only (the t01/A2 ground truth).** The trip host has no x86_64 musl
  linker, so the static `x86_64`/`aarch64-unknown-linux-musl` release binaries are a CI/release-job
  concern. Locally, the NATIVE release build (`cargo build --release --features host-daemon`) is
  verified; the release script (`deploy/release.sh`) + the CI `release-artifacts` job (which
  installs `musl-tools` + the cross-linkers) are provided, but no musl binary is faked locally.

## Scope of what built vs. parked

**Built + verified locally:**
- `cfs-host` wasm-clean core (default features) on `wasm32-unknown-unknown`.
- `cfs-host --features host-workers` (the parked scaffold) on `wasm32-unknown-unknown`.
- `cfs-host --features host-daemon` natively (the `FileDurableStore` + `AuditLedger` + the
  `ServerState`→`BindingSet` conversion).
- `cargo build --release --features host-daemon -p cfs` (the native release profile).
- The `cfs` binary composing `TokioHost` and writing the on-disk ledger from a real `cfs serve`.

**Parked (recorded honestly):**
- The `worker`-backed `WorkersHost`: the four `#[event]` entrypoints + the `#[durable_object]`
  class. The `DurableObjectStore` scaffold returns a structured `HostError` (never a panic) so a
  caller under `host-workers` fails CLEANLY until `worker` lands.
- The `cfs.wasm` cdylib artifact: `cfs-host` is a lib crate today; the `crate-type=["cdylib"]`
  Workers entrypoint crate is parked with `worker`. The release script records the parked state
  rather than faking a `.wasm` binary.
- The static musl binaries: emitted only by the CI release job (cross-link is CI-only).

## Tests require no live network or credentials

The host-agnostic binding-set assertion + the `wrangler.toml` golden boot a fixture `config.cfs`
into a `ServerState` through the in-memory t30 `Runtime` (parse→lower→COMMIT) and derive the
`BindingSet` — no network, no creds. The `MockHost` idempotency test drives a JOB and a WEBHOOK
twice through a `cas`-guarded in-memory cursor and asserts the committed effect set is identical
(at-least-once, RFD §6). The no-vendor-in-core deny-test resolves the full `cargo metadata` graph
and asserts `worker`/`hyper`/`axum`/AWS-storage SDKs are unreachable from `cfs-core`/`cfs-server`.

## Consequences

- **Pro**: one seam, owned DTOs, the daemon reuses the existing composition (zero rebuild), the
  wasm-clean core is provably buildable, and the CF host is a mechanical drop-in once `worker` is
  cached. The deny-test makes "vendor-free core" a mechanically enforced invariant.
- **Con**: the CF Workers host is not end-to-end runnable today (parked on the offline dep); we
  mitigate by pinning the archetype→primitive mapping in a tested scaffold and the
  `compile_error!` mutual-exclusion so the two hosts cannot be mis-built. The musl artifacts are
  unverified locally (CI-only) — the native release build is the local proxy. Both are recorded
  carry-overs, consistent with ADR-0002/0003/0004.
