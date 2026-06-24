# Review round-t22 — Architect (analytical)

Reviewer: Architect (Neutral / structural bridge)
Artifact reviewed: Constructor's t22 implementation — `crates/driver-objstore/` (commit `858e2b5`)
Design reference: `.workaholic/trips/qfs-foundation-e0/designs/design-vt22.md`
Ticket: `20260622214650-t22-driver-object-storage-s3-r2.md`
Method: code reading + architectural/model checking only (no test/build/clippy execution — that is Constructor internal QA + Planner E2E).

## Decision: Approve with observations

The crate is structurally sound and faithfully realizes the BlobNamespace archetype, the RFD §9 boundary invariant, and — most importantly — the truthful-pushdown-residual property that was the t20 failure mode. The hand-rolled crypto is a defensible, well-pinned structural choice. I am raising **three observations** (one is a genuine semantic-coherence gap worth a follow-up decision; two are SigV4-completeness limitations to record as scoped carry-overs), each with a constructive proposal. None blocks acceptance of t22 as scoped, because the live HTTP path is itself parked to t38 — but the irreversibility gap (Obs-1) should be a recorded carry-over with an owner, not silently inherited.

---

## 1. SigV4 correctness & the hand-rolled SHA-256/HMAC

**SHA-256 (`sha256.rs`)**: textbook FIPS 180-4 — correct padding (0x80, zero-fill to 56 mod 64, 64-bit BE length), correct K/H0 constants, correct message schedule and compression round. Pinned to the canonical FIPS KATs (empty, "abc", the 56-byte multi-block vector). **HMAC-SHA256 (RFC 2104)**: correct block size (64), correct long-key pre-hash, correct ipad/opad. Pinned to RFC 4231 Test Case 2 and a >block-size key path. These primitives are correct.

**SigV4 structure (`sigv4.rs`)**: canonical request (method / URI / query / headers / signed-headers / payload-hash), string-to-sign (`AWS4-HMAC-SHA256` / amz-date / scope / hash-of-canonical), and the four-step signing-key derivation (`AWS4`+secret → date → region → service → `aws4_request`) are all correct, and the **signing-key derivation is pinned to the AWS published byte vector** (20120215/us-east-1/iam) — the strongest single anchor available offline. Headers are lowercased, trimmed, sorted, and deduped; the query is name-sorted. The end-to-end GET test asserts the scope, the `host;x-amz-content-sha256;x-amz-date` signed-header set, and `UNSIGNED-PAYLOAD`. Determinism is asserted (no clock/nonce — the date is injected via `SigningContext`, which is the right purity seam).

**Verdict on hand-rolled crypto**: acceptable as a structural choice. It is used **only** for request signing over public canonical material (never at-rest encryption, never secret comparison), the non-constant-time caveat is correctly documented and genuinely irrelevant to the threat model (the signature goes on the wire), and it removes a `ring`/openssl native-link hazard on the wasm target. The correctness risk is bounded because the same offline vectors the ticket mandates pin it end-to-end. I would not ask for a dependency swap here.

**Observation 2 (SigV4 canonicalization completeness — scoped limitation, record as carry-over).** Two AWS-canonicalization steps are not implemented, and are not exercised by the simple-path vectors:
- `split_uri_query` takes the path **verbatim** ("already-encoded keys") and does **not** percent-encode the canonical URI. AWS SigV4 for S3 requires the canonical URI to be URI-encoded once (S3 is the single-encoding special case). A key containing a space, `+`, or other RFC-3986-reserved byte would currently sign a different canonical URI than S3 computes → `SignatureDoesNotMatch`.
- Query param **keys and values are not RFC-3986 percent-encoded** before sorting/joining (e.g. a `continuation-token` or `versionId` containing `/`, `+`, or `=` rides raw). AWS sorts by the *encoded* key and signs the *encoded* value.

Because the entire live HTTP backend is parked to t38 (no live S3/R2 call runs at E0), this does not break any E0 gate — but it is a real correctness gap the moment the signer faces a real key with a reserved character.
*Proposal*: record an explicit t38 sub-item "SigV4 RFC-3986 canonical-URI + query encoding (S3 single-encode rule), pinned to the AWS `get-vanilla-query`/`get-utf8`/`get-space` vectors from the SigV4 test suite." Add one offline vector with a spaced key now if cheap, so the limitation is captured as a failing-then-fixed marker rather than an unstated assumption. This is a translation-fidelity note: the doc comment says "canonical query" but the implementation is "sorted raw query," and the two should be reconciled in word or in code.

## 2. No vendor / `http::` / SigV4-internal leak past the public API (RFD §9 boundary)

Confirmed clean. `sigv4`, `sha256`, and `xml` are **private** modules (`mod`, not `pub mod`). The public re-exports (`lib.rs` 77–90) are all owned: `ObjApplier`, the backend trait + `HttpExchange`/`HttpRequest`-free DTO seam, `ByteStream`, `ListPage`/`ObjectMeta`/`PutResult`, `Multipart*`, `ObjNode`/`Scheme`, `ObjRegistry`, `ObjError`, and `SigV4Credentials`/`SigningContext` (which expose no `http::`/crypto type — `Secret` in, `&str` config in). `qfs_http_core::HttpRequest`/`HttpResponse` are used **inside** `backend.rs`/`sigv4.rs` but never re-exported; the `ObjectBackend` trait trades only owned DTOs + `ByteStream`. The `worker::Bucket` binding is held as an opaque `binding_name: String` marker (the `worker` crate is not even linked — see §5), so no vendor type crosses. The boundary invariant holds.

## 3. Pushdown residual truthfulness (the single most important property)

This is the strongest part of the change and the t20 lesson is correctly internalized. `key_prefix_of` (`lib.rs` 462–499) returns `(prefix, exact: bool)`, and `plan_ls` drops the residual **only** on `exact == true`:
- `key LIKE 'p%'` with the wildcard *only* at the tail and no embedded `%`/`_` → `(p, exact=true)` → residual dropped. Correct: the prefix list *is* the predicate.
- `key = 'x'` → `(x, exact=false)` → prefix pushed as a **superset**, exact `=` kept as residual. Correct and subtle: a `prefix=x` list also returns `xY`, so the `=` must re-filter. This is exactly the row-dropping trap t20 fell into, and it is avoided.
- `key BETWEEN lo AND hi` → common leading prefix, `exact=false` → residual kept. Correct.
- `key >= 'a'` / `> 'a'` → deliberately pushes **nothing** (a prefix superset of an ordering bound would exclude later keys) → whole residual kept. This is the right "correctness over cleverness" call and is documented as such.
- `AND` → pushes one conjunct's prefix but forces `exact=false` (`.map(|(p,_)| (p,false))`), keeping the **whole** predicate as residual. Correct: the other conjunct still constrains.
- No key constraint → push nothing, keep everything.

The tests (`ls_keeps_a_truthful_residual_for_a_partial_predicate`) assert the `=`, `AND`, and no-key cases keep the exact predicate. The structural property "the pushed prefix is always a superset filter; the residual is dropped only when provably exact" holds. No silent row-dropping path exists. **Approve.**

One small fidelity note (not a defect): the `PushdownProfile::Partial` advertises `limit: true`, but `ls`/`list_objects_v2` does not thread a `LIMIT` into the native `max-keys` query param — pagination is by continuation token only. The engine can still honor `LIMIT` locally, so this is truthful (over-claiming a *capability* the engine compensates for is safe, unlike under-keeping a residual), but if the planner trusts `limit: true` to mean "the driver caps the page natively," that is not yet realized. *Proposal*: either thread `LIMIT → max-keys` in the t38 live path, or downgrade `limit` to `false` until it is. Worth one sentence in the carry-over.

## 4. Token / credential safety

Solid. `secret_access_key` and `session_token` are `qfs_secrets::Secret`; the secret is `expose_str`'d **only** inside `derive_and_sign`/`sign` to compute the signing key and the redacted `Authorization` value. No DTO, no `ObjError` arm, and no `RecordedCall` carries a credential (every error arm is a path / verb-label / `&'static str` op / status / fixed reason). The planted-canary test drives all seven `ObjError` arms through `Debug` and `Display` and asserts neither the full secret nor the `deadbeef` fragment appears; `signed_request_debug_redacts_the_authorization` asserts the signed request's `Debug` contains `qfs_secrets::REDACTED` and not the secret. The structural guarantee (secrets confined to the signer, redacted on every observable surface) holds. **Approve.**

Minor: `derive_and_sign` uses `expose_str().unwrap_or_default()` — an empty secret would silently produce a (wrong) signature rather than erroring. Harmless for safety (no leak), but a misconfiguration would surface only as a server-side 403. *Proposal (optional)*: in the t38 live path, treat an empty exposed secret as a structured `ObjError` at backend construction rather than signing with `AWS4` + "".

## 5. wasm gating soundness

The gating is **structurally sound** as a park, and the carry-over is the right resolution. `R2BindingBackend` and its `ObjectBackend` impl are entirely under `#[cfg(target_arch = "wasm32")]`, and the `worker` crate is a `[target.'cfg(...)']` dependency — so the native build provably never links it (confirmed: the type, its constructor, and its trait impl are all behind the cfg, and no native code references it; native `/r2` reuses `Backend::Http`). The binding methods are honest stubs returning a structured `ObjError::Api { status: 501 }` ("binding unavailable") rather than `unimplemented!()`, which keeps the wasm core type-checkable without a live Workers lane.

The recorded assumption — that the **whole** runtime-leaf crate does not build standalone on `wasm32` because `qfs-runtime` pulls `tokio` with `rt-multi-thread` — is a **real, shared workspace constraint** (identical to t23's `qfs-driver-cf`), not a t22-specific defect. The binding *core* (signer, SHA-256/HMAC, DTOs, `ByteStream`, path/multipart/xml, `backend.rs`'s cfg'd `R2BindingBackend`) being wasm-clean in isolation is the correct and sufficient proof that the binding type-checks and the native build stays clean. This is a sound park, not a coherence gap. *Proposal*: I endorse the carry-over (a thin wasm Workers entrypoint crate composing the binding modules without the tokio bridge) and recommend it be filed against t38 with the t23 entrypoint so the two driver families share one wasm composition root rather than diverging.

## 6. Archetype / capability / effect-plan coherence

Mostly coherent, with **one genuine semantic gap (Observation 1)**.

Coherent: `BlobNamespace` archetype on bucket+object nodes (root is correctly non-describable); parse-time capability gating via `caps_for` + the driver `check_capability` helper produces a structured `UnsupportedVerb` naming the node and the allowed set (test-asserted on a bucket-root `UPDATE`); the effect mapping (`UPSERT`→put single/multipart, `REMOVE`→delete with optional `@versionId`) is faithful; the copy→verify→delete legs are exposed as primitives (`copy_leg`/`verify_leg`/`delete_leg`) and explicitly **not** orchestrated here, matching the ticket's "expose the leg primitives the planner composes."

**Observation 1 (irreversibility semantics — versioning-aware reversibility is claimed but not wired).** The design (§"Effect mapping + irreversibility") and the `effect.rs`/`registry.rs` docstrings state that a plain `REMOVE` on a **versioned** bucket inserts a *recoverable* delete-marker and the effect node's `irreversible` flag should "reflect the bucket's versioning." But the plan layer (`crates/plan/src/node.rs::is_inherently_irreversible`) marks **every** `Remove` irreversible at the **verb** level, and this crate — which is the only component that *knows* `Bucket::is_versioned()` — never feeds that knowledge into node construction. So a plain REMOVE on the versioned `archive` bucket is flagged irreversible even though it is recoverable, and a specific-`@versionId` REMOVE on a non-versioned bucket is treated the same as a recoverable delete-marker. The driver does expose the distinction through `version_support()` (`Versioned`/`Snapshot`/`None`), but nothing consumes it to refine reversibility.

This is conservative-safe (it never *under*-warns: it can only over-warn "irreversible" on a recoverable delete), so it is not a correctness hazard and does not block t22. But it is a **translation-fidelity gap**: the design promises a versioning-aware `irreversible` flag the system does not actually compute, and the words should match the behavior.
*Proposal (pick one, record the choice):*
(a) **Reconcile the docs to the behavior now** — restate that at E0 every REMOVE is conservatively irreversible regardless of bucket versioning, and that versioning-aware refinement is a t38 carry-over. Cheapest, removes the false promise.
(b) **Wire the refinement** — give the planner/effect-builder a hook to consult `Driver::version_support(path)` (already public) so a plain REMOVE on a `Versioned` bucket is built with `irreversible=false`. Structurally the seam already exists; only the planner-side wiring is missing. File as t38 if not done now.
Either is acceptable; what matters is that the gap is recorded with an owner rather than inherited as an unstated divergence.

## 7. Dependency direction (runtime leaf, no spine inversion)

Confirmed. `qfs-driver-objstore` is appended to `runtime_consumers_allowed` in `crates/cmd/tests/dep_direction.rs` (the one-line reviewable signal). The crate bridges its synchronous `PlanApplier`/`SharedApplier` to the async `ApplyDriver` via `PlanApplierBridge` and nothing depends back onto it (tokio dead-ends at the leaf), so the spine is not inverted. It rides the local `HttpExchange` seam over `qfs-http-core` rather than depending on `qfs-driver-http` — preserving the independent-leaf property and matching the established `qfs-google-auth`/`qfs-driver-cf` precedent. The `worker` crate is correctly a target-cfg dependency. **Approve.**

## Cross-cutting coherence assessment

The crate is internally consistent: one shared `ObjDriver` behind `S3Driver`/`R2Driver` newtypes differing only at the `Scheme` edge, a single `ObjectBackend` transport seam with three interchangeable impls (`HttpBackend` real, `MockObjectBackend` test, `R2BindingBackend` parked) all producing identical owned DTOs, and a single `dto::*_COL` constant set shared between the schema and the row projection so the two cannot drift (schema test pins the column order/nullability). `Driver::id()` derives `s3`/`r2` from the mount by the default impl (test-confirmed for r2). The streaming invariant is honored structurally — `ByteStream` chunks bounded at `DEFAULT_MAX_CHUNK`, multipart cuts at `part_size`, and `into_bytes` is the explicit opt-in materialization point — though at E0 the source effect still carries a full `Vec<u8>` body (correctly disclosed in the applier docstring; true end-to-end streaming is a transport-adapter concern for t38).

The three observations are a recoverable-delete semantic gap (Obs-1, the one to actually decide on) and two SigV4 completeness limits (Obs-2 URI/query encoding, plus the `limit` over-claim) — all of which live behind the t38-parked live HTTP path and none of which compromises the E0 gates (offline SigV4 vectors, plan-shape goldens, mocked S3 behavior, token canary, dep-direction, no-vendor-leak). I therefore approve t22 as scoped, conditioned on the three items being recorded as t38 carry-overs (Obs-1 with an explicit doc-or-wire decision, Obs-2 with the AWS encoding vectors).

---
Status: under-review → Approve with observations.
