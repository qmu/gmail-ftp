---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260622214650-t13-driver-contract-trait.md]
---

# Driver: object storage (AWS S3 + Cloudflare R2)

## Overview

Delivers the **blob/namespace** driver(s) for S3-compatible object storage, mounted at
`/s3/<bucket>/<key>` (AWS S3) and `/r2/<bucket>/<key>` (Cloudflare R2). This realizes the
"Blob / namespace" archetype from RFD §5 (native verbs `ls cp mv rm`) and proves the
universal-CRUD claim of RFD §2/§4: an object is created with `UPSERT INTO /s3/bucket/key`
(no per-driver create verb), listed via `FROM /s3/bucket/...`, fetched by reading the path,
and deleted with `REMOVE`. It also exercises the `@version` temporal coordinate
(`/s3/bucket/key@<versionId>`, RFD §4) and the codec bridge (RFD §4): an S3 object is just a
blob source that `DECODE json|yaml|csv|…` turns into rows.

S3 and R2 share one S3-compatible HTTP core (SigV4, thin client per RFD §9 — no vendor SDK).
The split is at the edge: on EC2/CLI both go over HTTPS; compiled to `wasm32` for Workers,
R2 is served via the **native R2 binding** (RFD §8 deployment mapping: `/r2` → binding,
not HTTP), selected at runtime behind the same `Driver` trait.

## Scope

In scope:
- `S3Driver` + `R2Driver` implementing the `Driver` trait from ticket t13.
- Namespace/archetype declaration, per-node schema (`DESCRIBE`), capabilities (which verbs
  each path node supports), and pushdown declaration (prefix listing, range GET).
- `ls` (list objects, prefix + delimiter), `get` (streaming download), `UPSERT` (put,
  single + multipart), `REMOVE` (delete object/version), streaming bodies both directions.
- `@versionId` addressing for GET/REMOVE on versioned buckets.
- SigV4 request signer; R2 native binding path under `wasm32`.
- Owned DTOs for object/list/version metadata (no vendor types past the boundary).

Out of scope (deferred):
- The `Driver` trait, archetype/capability/effect-plan types — **t13** (dependency).
- Credential store / SigV4 secret sourcing & encryption — **E5 auth ticket** (this ticket
  consumes an injected credential provider; it does not own secret storage).
- Codec registry (`DECODE`/`ENCODE`) — separate E3 codec ticket; this driver only emits/accepts bytes.
- Cross-source `cp`/`mv` orchestration & ledger recovery — **E2 runtime ticket**; here we
  expose the leg primitives (copy→verify→delete) the planner composes.
- `/d1`, `/kv` and other CF bindings — their own E4 tickets.

## Key components

New crate-internal module `drivers::objstore` (per RFD §9 "consumer-side small traits",
directory `src/drivers/objstore/`):

- `mod s3client` — thin HTTP+SigV4 client. `struct S3Client { endpoint, region, creds: Arc<dyn CredentialProvider> }`.
  `sign_v4(&self, req: &mut http::Request)`; methods `list_objects_v2`, `get_object`
  (returns `ByteStream`), `put_object`, `create_multipart`, `upload_part`, `complete_multipart`,
  `abort_multipart`, `delete_object`, `copy_object`. No public re-export of `http`/vendor types.
- Owned DTOs (`mod dto`): `ObjectMeta { key, size, etag, last_modified, version_id: Option<String>, storage_class }`,
  `ListPage { objects: Vec<ObjectMeta>, common_prefixes: Vec<String>, next_token: Option<String> }`,
  `PutResult { etag, version_id: Option<String> }`. `Serialize`-able rows for the engine.
- `enum Backend { Http(S3Client), R2Binding(R2BindingHandle) }` — runtime selection;
  `R2BindingHandle` is `#[cfg(target_arch = "wasm32")]` over `worker::Bucket`, gated so the
  native build never links it.
- `struct S3Driver(Backend)` / `struct R2Driver(Backend)` implementing `Driver`:
  - `fn describe(&self, path: &Path) -> NodeSchema` (bucket node, key node).
  - `fn capabilities(&self, path: &Path) -> Caps` — key node: `LS|GET|UPSERT|REMOVE`;
    bucket root: `LS|UPSERT`; rejects unsupported verbs at parse time (structured error, RFD §5).
  - `fn plan_read(&self, path, pipe) -> Result<PlanNode>` — pure; `ls`/`get` → effect-free
    read node (RFD §3 purity invariant: constructs, never performs).
  - `fn plan_write(&self, verb, path, rows) -> Result<PlanNode>` — `UPSERT`/`REMOVE` →
    effect node carrying `irreversible` flag (REMOVE on non-versioned bucket = irreversible).
- `mod multipart` — `Multipart { upload_id, parts: Vec<PartEtag> }`, part-size policy
  (default 8 MiB, threshold to switch from single PUT), `commit`/`abort`.
- `mod versioned` — parse `key@<versionId>` into `(key, Option<VersionId>)`; thread into
  GET/REMOVE; emit `version_id` on listing of versioned buckets.

## Implementation steps

1. Scaffold `src/drivers/objstore/` with `dto`, `s3client`, `multipart`, `versioned`, `driver` submodules.
2. Implement SigV4 signer (canonical request, signed headers, streaming UNSIGNED-PAYLOAD for large/streamed bodies); unit-test against AWS published test vectors (no network).
3. Implement `S3Client` HTTP methods over the project's thin async HTTP client; map errors to the engine's structured error enum.
4. Define owned DTOs and the `ObjectMeta → Row` projection (key, size, etag, last_modified, version_id columns) powering `DESCRIBE`/`ls`.
5. Implement `Driver` for `S3Driver`: path parsing (`bucket`, `key`, `key@versionId`), `capabilities`, `describe`, `plan_read`, `plan_write`.
6. Implement multipart upload (create/upload-part/complete) with abort-on-error and configurable part size; route `UPSERT` to single PUT below threshold, multipart above; stream the body.
7. Implement streaming GET (range support for pushdown) returning a `ByteStream` the runtime pipes into codecs or files.
8. Add versioned-bucket support: list versions, GET/REMOVE by `@versionId`, ETag surfaced for optimistic concurrency (RFD §6).
9. Add `R2Driver`: reuse `Backend::Http` for native builds; under `#[cfg(target_arch = "wasm32")]` implement `Backend::R2Binding` over `worker::Bucket` (get/put/list/delete/multipart) with the same DTOs.
10. Register both mounts (`/s3`, `/r2`) in the path registry (RFD §3 open "paths" namespace); declare pushdown (prefix list, byte-range GET).
11. Wire copy primitive (`copy_object`) and expose the copy→verify→delete leg for the runtime's cross-source `cp`/`mv` (RFD §6) — do not orchestrate here.
12. Tests: mock-HTTP integration (wiremock), plan-shape golden tests, signer vectors, multipart sequencing.

## Considerations

- **Least-privilege & secrets** (RFD §10): the driver never reads env/disk for keys; it
  takes `Arc<dyn CredentialProvider>` injected by the auth layer, and credentials/Authorization
  headers are **never logged**. Capability gating (RFD §5) plus server `POLICY` decide which
  buckets/verbs a handler may touch; the driver only enforces verb-per-node.
- **Idempotency & recovery** (RFD §6): `UPSERT` (PUT) is naturally retry-safe (at-least-once
  webhooks). Multipart must `abort_multipart` on failure to avoid orphan-part billing —
  treat the upload_id as recoverable state in the effect node. `REMOVE` is idempotent;
  optimistic concurrency for read-then-write via `If-Match`/ETag and `@versionId`.
- **Streaming, not buffering**: large objects must stream end-to-end (bounded memory) — this
  is the genuinely hard part on `wasm32` where R2 binding streams differ from `hyper`
  bodies; abstract behind a `ByteStream` so the engine is backend-agnostic.
- **Purity invariant** (RFD §3): `plan_*` only constructs `PlanNode`s; the single I/O point is
  `COMMIT`. No method on the read path performs network I/O eagerly.
- **No vendor leak** (RFD §9): `worker::Bucket`, `http::Response`, SigV4 internals stay inside
  the module; only owned DTOs and `ByteStream` cross the boundary.
- **Observability** (RFD §6): per-leg timeout, bounded retry with backoff on 5xx/throttling
  (S3 `SlowDown`/503), circuit breaker; every applied effect (put/delete/multipart-complete)
  recorded in the audit ledger with bucket/key/version_id (never the body).
- **wasm gating**: native and wasm builds must both compile; `#[cfg]` the binding so neither
  pulls the other's deps. CI builds both `--target` outputs.

## Acceptance criteria

- `cargo build` (native) and `cargo build --target wasm32-unknown-unknown` both green;
  `cargo clippy -- -D warnings` clean; `cargo test` green with **no live credentials**.
- SigV4 signer reproduces AWS published test vectors (unit, offline).
- **Plan assertions** (golden): `UPSERT INTO /s3/b/k …` and `REMOVE /s3/b/k@v` produce the
  expected effect-plan DAG (correct verb, `irreversible` flag, version_id), and `FROM /s3/b/...`
  produces an effect-free read node — asserted without any network call.
- Against a mock S3 (wiremock): `ls` returns paged `ObjectMeta` rows with prefixes; `get`
  streams bytes for single + ranged requests; `UPSERT` below threshold = single PUT, above =
  multipart with `complete`; injected failure mid-multipart triggers `abort`.
- Capability rejection: an unsupported verb on a bucket-root node fails at parse time with a
  structured error naming the node and allowed verbs.
- `@versionId` GET/REMOVE round-trips against the mock; ETag surfaced for optimistic concurrency.
- DTO boundary check: no `worker::`, `http::`, or SigV4 type appears in the module's public API
  (verified by a `pub`-surface review / doc check).
