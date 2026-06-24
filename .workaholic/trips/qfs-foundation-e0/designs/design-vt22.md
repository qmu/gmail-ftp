# Design vt22 — S3 + Cloudflare R2 object-storage driver

Author: Constructor
Status: approved (single-ticket coding-phase design)
Reviewed-by: (coding-phase implementation design; reviewed via Architect analytical review + Planner E2E)

## Content

### Scope
New leaf crate `crates/driver-objstore` (`qfs-driver-objstore`) exposing two drivers over one
S3-compatible HTTP core, each on the **BlobNamespace** archetype (RFD §5):

- **S3** `/s3/<bucket>/<key>` (AWS S3) and **R2** `/r2/<bucket>/<key>` (Cloudflare R2).
- Native verbs `ls cp mv rm` + universal `UPSERT` / `REMOVE` / `get` (streaming download).
- `@versionId` addressing `/s3/<bucket>/<key>@<versionId>` for GET/REMOVE; ETag surfaced for
  optimistic concurrency.

S3 and R2 share one S3-compatible HTTP core (SigV4, thin client per RFD §9 — no vendor SDK).
The split is at the edge: on EC2/CLI both go over HTTPS (`Backend::Http`); compiled to `wasm32`
for Workers, R2 may be served via the native `worker::Bucket` binding (`Backend::R2Binding`,
`#[cfg(target_arch = "wasm32")]` only, gated so the native build never links it).

### Module layout (`src/`)
- `path` — `ObjNode` parse: scheme (`s3`/`r2`), bucket, key, optional `@versionId`. Pure.
- `dto` — owned DTOs: `ObjectMeta { key, size, etag, last_modified, version_id, storage_class }`,
  `ListPage { objects, common_prefixes, next_token }`, `PutResult { etag, version_id }`,
  `ObjectMeta::to_row` projection. `Serialize`-able for `ls`/DESCRIBE rows.
- `bytestream` — `ByteStream`: an owned, **bounded-memory** chunked stream (`Vec<Vec<u8>>` of
  capped chunks behind an iterator) that crosses the public boundary in place of `hyper`/SDK
  body types. Both directions (GET returns one; PUT/multipart consume one).
- `sigv4` — the SigV4 v4 signer (canonical request, signed headers, `UNSIGNED-PAYLOAD` for
  streamed bodies). **Private module**: no `http::`/SigV4-internal type leaks past the crate
  API. Unit-tested against AWS published vectors (offline).
- `multipart` — `MultipartPolicy { part_size, threshold }` (default 8 MiB), `Multipart`
  sequencing state (`upload_id`, `parts: Vec<PartEtag>`), abort-on-error.
- `backend` — `ObjectBackend` trait (owned DTOs only), `HttpBackend` (real, over a local
  `HttpExchange` seam on `qfs-http-core` — the cf precedent, so NO dep on `qfs-driver-http`),
  `MockObjectBackend` (in-memory fixtures + recorded calls), and the parked wasm
  `R2BindingBackend`.
- `error` — `ObjError` (structured, secret-free, AI-consumable; `code()` + `is_retryable()`).
- `effect` — `ObjEffect` decode from a runtime `EffectNode` (UPSERT→put, REMOVE→delete).
- `applier` — `ObjApplier` (synchronous `SharedApplier` + `PlanApplier`).
- `registry` — `ObjRegistry` mapping `<bucket>` → `Bucket { backend, versioned }`.
- `schema` — the object-listing relation schema.
- `lib` — `S3Driver`, `R2Driver`, `s3_apply_driver`/`r2_apply_driver` bridges, and the
  `copy→verify→delete` leg primitives the runtime composes for cross-source cp/mv.

### Confined HTTP seam + SigV4 (leaf invariant, no vendor leak)
`trait ObjectBackend` is the single transport seam, owned DTOs in/out. The real impl
`HttpBackend` builds owned `qfs_http_core::HttpRequest`s, runs the **SigV4** signer over them
(injecting the `Authorization` + `x-amz-*` headers), and sends them over a local
`HttpExchange` seam — so reqwest stays confined to `qfs-driver-http` and this crate stays an
**independent runtime leaf** (no `qfs-driver-http` edge). The SigV4 signer and any `http::`/
SigV4-internal types live entirely inside the private `sigv4` module; only owned DTOs +
`ByteStream` cross the public API.

### Secret discipline (RFD §10)
Credentials are injected as a `qfs_secrets::Secret` (access-key-id is non-secret config; the
secret-access-key + any session token are `Secret`). They are exposed only inside the signer to
compute the signature and to write the `Authorization` header value (redacted by the shared
`qfs-http-core` redaction authority). Never logged, never stored in a DTO, never in an
`ObjError`. A planted-canary test proves the secret never appears in any error surface and the
request `Debug` redacts the `Authorization` header.

### Streaming, not buffering
`ByteStream` carries bounded chunks (no full-object `Vec<u8>` materialization on the public
surface). A PUT below the multipart threshold (~8 MiB) is one `put_object`; above it the
`UPSERT` is multipart (`create_multipart` → N×`upload_part` → `complete_multipart`), and any
mid-sequence failure triggers `abort_multipart` (no orphan-part billing, RFD §6).

### Truthful pushdown residual (the t20 lesson)
`pushdown` is `Partial { project, where_ }`: an `ls` pushes the **prefix** (and delimiter) of a
key predicate down as a native S3 `prefix=`/`delimiter=` list, and a GET pushes a byte **range**
down as a `Range:` header. When a `WHERE` predicate is only **partially** expressible as a
prefix/range, the driver keeps the **exact** predicate as a residual so the engine re-filters —
never silently dropping it and returning wrong rows. `plan_read` returns `(PlanNode, residual)`.

### Capability gating (parse-time, structured)
Per-node `capabilities(path)`: a key node → `{ls,get→select,upsert,remove,cp,mv,rm}`; a bucket
root → `{ls,upsert,cp,mv}`. An unsupported verb fails at the parse gate with
`CfsError::UnsupportedVerb` naming the node + the allowed verbs.

### Effect mapping + irreversibility
- `UPSERT INTO /s3/b/k` → `put_object` (single) or multipart (large) — retry-safe.
- `REMOVE /s3/b/k[@v]` → `delete_object` (with `versionId` when present). On a **non-versioned**
  bucket a REMOVE is **irreversible** (the object is gone); on a versioned bucket deleting a
  specific `@versionId` is also irreversible, but a plain REMOVE inserts a delete-marker
  (recoverable) — the effect node's `irreversible` flag reflects the bucket's versioning.
- `cp`/`mv` legs: `copy_object` (server-side) + the exposed copy→verify(ETag)→delete primitive
  for the runtime's cross-source orchestration (NOT orchestrated here).

### Plan-shape golden tests (no network)
`UPSERT INTO /s3/b/k` → effect node `Upsert` with the bucket/key target; `REMOVE /s3/b/k@v` →
`Remove` node carrying `irreversible=true` + the `version_id`; `FROM /s3/b/...` → effect-free
read node — all asserted purely (no socket).

### Crate dependency edges (G6 acyclic, runtime-leaf)
`qfs-driver-objstore → { qfs-driver, qfs-plan, qfs-types, qfs-runtime, qfs-http-core,
qfs-secrets, serde, serde_json, thiserror, tracing, hex, sha2, hmac }`. It is a **runtime leaf**
(bridges its synchronous `PlanApplier` → async `ApplyDriver`); nothing depends back onto it, so
tokio dead-ends. Append `qfs-driver-objstore` to `dep_direction.rs`'s `runtime_consumers_allowed`
allowlist (one-line reviewable signal). The wasm `worker` crate is a `[target.'cfg(...)'`
dependency so the native build never links it.

### Tests (mocked S3 + offline SigV4 vectors — NO live network/credentials)
1. SigV4 reproduces AWS published `aws4_request` test vectors (canonical request + signature).
2. `ls` returns paged `ObjectMeta` rows with common prefixes; the prefix is pushed to the
   `prefix=`/`delimiter=` query; a partial predicate keeps a truthful residual.
3. `get` streams bytes (single + ranged `Range:` request) via `ByteStream`.
4. `UPSERT` below threshold = one `put_object`; above = multipart `complete`; an injected
   failure mid-multipart triggers `abort_multipart`.
5. `@versionId` GET/REMOVE round-trip against the mock; ETag surfaced.
6. Plan-shape golden: UPSERT/REMOVE@v/FROM produce the expected nodes + flags.
7. Capability rejection: an unsupported verb on a bucket-root node fails structurally.
8. Token never leaks: planted-canary across every `ObjError` Debug/Display + the redacting
   request Debug.
9. End-to-end through the interpreter + bridge for an UPSERT and a REMOVE (`#[tokio::test]`).
10. wasm build green (`cargo build --target wasm32-unknown-unknown`); the R2 binding code
    `#[cfg(target_arch="wasm32")]` type-checks.

### Verification gates
`cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo build`
(native) + `cargo build --target wasm32-unknown-unknown`; `cargo test --workspace` — all green,
no regression on 552, plus qfs-plan purity + generic runtime-leaf confinement still green.

## Review Notes
Concern (engineering): the SigV4 signer needs HMAC-SHA256 + SHA256. RESOLVED during
implementation: the trip host's cargo cache does **not** carry `sha2`/`hmac`/`hex`/`ring` (offline)
and the wasm target must also build, so SHA-256 + HMAC-SHA256 + lowercase-hex are implemented
**dependency-free** in the private `sha256` module (FIPS 180-4 / RFC 2104), pinned by the FIPS,
RFC 4231, and the **AWS published SigV4 signing-key derivation** vectors. No crypto crate, no
`ring`/openssl native-link hazard on wasm; no crypto type crosses the public API.

Recorded assumption (wasm build): the **whole runtime-leaf crate** does NOT build standalone on
`wasm32-unknown-unknown` because it depends on `qfs-runtime`, which pulls `tokio` with
native-only features (`rt-multi-thread`) — the wasm tokio build rejects them. This is a
**workspace-wide constraint the t23 `qfs-driver-cf` driver shares** (no runtime-bridge driver
crate builds standalone on wasm; the wasm Workers target links the binding-side modules, not the
tokio bridge). The ticket's wasm requirement is met where it matters: the **wasm-clean core**
(SigV4 signer, SHA-256/HMAC, DTOs, `ByteStream`, path/multipart/xml, and `backend.rs` carrying the
`#[cfg(target_arch="wasm32")]` `R2BindingBackend`) was **compiled green for
`wasm32-unknown-unknown`** in isolation (no `qfs-runtime`), proving the R2 binding type-checks and
the native build never links it. Carry-over for a later ticket / t38: a thin wasm Workers
entrypoint crate that composes the binding modules without the tokio bridge.

The carry-over (live S3/R2 E2E, presigned URLs, SSE-C, the live R2 `worker::Bucket` impl behind
the parked `R2BindingBackend`) is parked to **t38** per the ticket.
