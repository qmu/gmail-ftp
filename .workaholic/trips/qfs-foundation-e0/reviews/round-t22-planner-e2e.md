# Review round-t22 — Planner (E2E / external-interface testing)

Reviewer: Planner (Progressive / business outcome + stakeholder advocacy)
Artifact validated: Constructor's t22 implementation — `crates/driver-objstore/` (commit `858e2b5`); Architect approved with observations (`a3da67b`).
Ticket: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t22-driver-object-storage-s3-r2.md`
Method: BLACK-BOX behavioral validation through the crate's PUBLIC API — a new, independent integration-test harness `crates/driver-objstore/tests/planner_e2e.rs` (16 scenarios) that I authored and drove, plus observation of the offline SigV4 published-vector unit anchor. NO live S3/R2, NO network, NO live credentials — E2E here is executable black-box scenarios against the in-memory mock backend + the runtime interpreter/bridge, exactly as t16/t17/t20/t23 were validated.

This is NOT a code review and NOT a re-run of the Constructor's `src/tests.rs` as the deliverable. My deliverable is `tests/planner_e2e.rs`: an external test binary that exercises only the public surface (`qfs_driver::Driver`, `ObjDriver`/`S3Driver`/`R2Driver`, the `MockObjectBackend`, the `qfs_runtime` interpreter), and several scenarios actively try to BREAK the truthful-pushdown-residual and token-safety properties rather than re-assert the happy path.

## Decision: Approve (7/7 acceptance scenarios validated)

All seven acceptance-criteria scenarios are validated end-to-end through the public interface. 16/16 of my external E2E scenarios pass; the crate's full suite (45 in-crate + 16 external) is green; clippy `-D warnings` is clean on the test binary. I raise **one observation** (the wasm-target build criterion) with a constructive proposal — it is a documented, Architect-confirmed shared-workspace park (identical to the accepted t23), not a t22 behavioral regression, so it does not block.

### How to reproduce
```
source ~/.cargo/env
cargo test  -p qfs-driver-objstore --test planner_e2e     # 16 external E2E scenarios — all pass
cargo test  -p qfs-driver-objstore                          # full suite: 45 unit + 16 E2E — all pass
cargo test  -p qfs-driver-objstore --lib sigv4              # 5 offline SigV4 tests incl. the AWS vector
cargo clippy -p qfs-driver-objstore --tests -- -D warnings  # clean
```

---

## Scenario-by-scenario results (mapped to the ticket's acceptance criteria)

### 1. Plan-shape golden (no network) — PASS
`s1_upsert_remove_and_read_plan_shapes`. Driven through the public effect/plan surface:
- `UPSERT INTO /s3/assets/k` → an `Upsert` effect node with `irreversible == false` (retry-safe).
- `REMOVE /s3/archive/doc@v9` → a `Remove` node with `irreversible == true`; `ObjEffect::from_node` decodes the `@v9` coordinate into `Delete { version_id: Some("v9") }` — the version survives the path→effect translation.
- `FROM /s3/assets` (`List`) → an effect-free read node, `irreversible == false`.

Shapes (verb, irreversible flag, version_id) are exactly as the ticket mandates.

### 2. SigV4 signer reproduces the AWS published vector offline — PASS (observed)
The signer is a PRIVATE module (no vendor leak, see scenario 7/boundary), so the published-byte-vector anchor correctly lives in the in-crate unit layer. I observed `sigv4::tests::signing_key_matches_aws_published_derivation` reproduce the AWS-documented 32-byte signing key for `20120215 / us-east-1 / iam` with the example secret — the strongest single offline anchor. The end-to-end `signs_a_get_with_correct_scope_and_signed_headers`, `signing_is_deterministic`, and `canonical_query_is_name_sorted` all pass. From the outside, my `s2_no_signer_or_crypto_type_crosses_the_public_boundary` confirms the public driver answers `get` through owned DTOs needing none of the signer types — the boundary that keeps the vector private holds.

### 3. Mock S3 behavior — PASS
- `s3_ls_returns_paged_rows_and_common_prefixes`: `ls` returns 2 `ObjectMeta` rows + the `logs/2026/` common prefix + `next_token` (pagination), and projects to the declared listing row order. The pushed prefix/delimiter (`logs/`, `/`) are observed in the recorded backend call.
- `s3_get_streams_single_and_ranged`: full GET returns all bytes; a ranged GET `(4,7)` returns exactly `4567` and the byte range is observed pushed down as the recorded `Get { range: Some((4,7)) }`.
- `s3_upsert_below_threshold_is_one_put_above_is_multipart_complete`: a 5-byte body = exactly one `Put`; a 10-byte body under a 4/4 policy = `CreateMultipart` → 3× `UploadPart` → `CompleteMultipart`, with NO abort.
- `s3_mid_multipart_failure_triggers_abort`: an injected failure at part 2 yields a terminal structured error, emits an `AbortMultipart` (orphan-part cleanup), and crucially NO `CompleteMultipart` fires after the failure.

### 4. `@versionId` GET/REMOVE round-trips; ETag surfaced — PASS
`s4_version_id_get_and_remove_round_trip_with_etag`: GET `/s3/archive/doc.txt@v7` records `version_id: "v7"`; REMOVE `/s3/archive/doc.txt@v3` records `Delete { key: "doc.txt", version_id: "v3" }`. The `PutResult` ETag is surfaced and drives the copy→verify leg: a matching ETag passes, a mismatch is a structured `conflict`. `version_support` correctly reports `Versioned` (archive), `Snapshot` (assets), `None` (unregistered).

### 5. Capability rejection at parse time, structured, naming node + verbs — PASS
`s5_unsupported_verb_on_bucket_root_is_structurally_rejected`: `Update`, `Remove`, AND `Rm` on a bucket root `/s3/assets` each fail with `code() == "unsupported_verb"`, a `CfsError::UnsupportedVerb { path: "/s3/assets", supported: [..] }` that NAMES the node and lists allowed verbs (LS/UPSERT present, RM absent). A key node admits the full blob set; an unregistered bucket has the empty capability set (everything rejected).

### 6. Pushdown residual truthfulness — PASS, and I actively tried to BREAK it (the highest-value check)
This is the property that has bitten the team before (t20). Three external tests, with adversarial predicates designed to produce wrong rows if the residual were ever silently dropped:

`s6_residual_is_kept_whenever_the_prefix_is_a_strict_superset` enumerates seven traps:
- (a) `key = 'logs/exact.json'` → prefix is a SUPERSET (also matches `...jsonX`) → exact `=` residual KEPT. ✓
- (b) `AND(key LIKE 'img/%', size > 1000)` → push `img/`, KEEP the whole predicate (the size conjunct still constrains). ✓
- (c) `BETWEEN 'apple' AND 'apricot'` → common prefix `ap` (superset, also `april`) → residual KEPT. ✓
- (d) `key >= 'm'` → an ordering bound is NOT a prefix; pushing `m` would EXCLUDE later `z...` rows. Driver pushes NOTHING, keeps the whole residual (the inverse row-EXCLUSION trap). ✓
- (e) non-key column predicate → push nothing, keep everything. ✓
- (f) **`NOT(key LIKE 'tmp/%')`** → pushing `tmp/` would return EXACTLY the rows to exclude. Driver pushes NO prefix, keeps the full predicate. ✓
- (g) **`key LIKE 'a/%' OR key LIKE 'b/%'`** → pushing either single prefix would DROP the other branch's rows. Test PANICS if any prefix is pushed for an OR; driver pushes none, keeps the whole predicate. ✓

`s6_residual_is_dropped_only_for_an_exact_prefix_like`: the residual is dropped ONLY for a tail-anchored LIKE with no embedded/leading wildcard (`logs/2026/%`). An embedded-wildcard (`logs/%/2026`) and a leading-wildcard (`%foo`) LIKE keep the residual or push nothing.

`s6_returned_residual_actually_filters_the_over_returned_rows`: the EXECUTED "no wrong rows" contract — the mock over-returns 3 rows under prefix `logs/exact.json` (`...json`, `...json.bak`, `...jsonX`); I apply the handed-back `=` residual and confirm re-filtering yields EXACTLY the 1 exact match. The residual is not just present, it is *sufficient* to recover correctness.

I could not break it. The driver never silently drops a predicate and never pushes a prefix that excludes a kept row. This is the strongest part of the change.

### 7. Token safety — PASS
`s7_no_canary_in_any_error_display_or_debug`: every public `ObjError` arm through `Debug` + `Display` — my planted canary `PLANTED-CANARY-cafef00d-...`, its `cafef00d` fragment, and even the literal `secret` appear in NONE.
`s7_no_canary_in_list_results_plan_or_recorded_calls`: a full ls+get round, then the serialized list-page JSON (the `-json` an operator sees), the `ListPushdown` Debug (the inspectable plan), and the recorded backend calls — the canary is absent from all three observable surfaces. (Corroborated by the in-crate `the_credential_never_appears_in_any_error_surface` and `signed_request_debug_redacts_the_authorization`, which I observed pass.)

### Cross-cutting — end-to-end COMMIT through the runtime — PASS
`e2e_commit_upsert_and_remove_through_the_s3_bridge`: a 2-effect plan (UPSERT + REMOVE@v1) committed through the real `Interpreter` + `PlanApplierBridge` with a `CapabilitySet` grant completes fully and records a `Put` and a `Delete{version_id:"v1"}`. `e2e_r2_commits_through_its_own_bridge_id`: the R2 driver derives its own `r2` id and commits an UPSERT through its own bridge. Both schemes work through the same shared core.

---

## Observation (non-blocking) — the `cargo build --target wasm32-unknown-unknown` acceptance line

`cargo build -p qfs-driver-objstore --target wasm32-unknown-unknown` does NOT build standalone: `qfs-driver-objstore → qfs-runtime → tokio (rt-multi-thread)` triggers `tokio`'s `compile_error!("Only features sync,macros,io-util,rt,time are supported on wasm.")`. This is NOT a t22 regression and NOT a behavioral defect I can exercise:
- It is a **shared workspace constraint** identical to the already-accepted t23 `qfs-driver-cf` (both are runtime leaves that bridge through `qfs-runtime`).
- It is **explicitly disclosed** in `lib.rs` "Named parks" and confirmed by the Architect's analytical review §5 ("a real, shared workspace constraint… not a t22-specific defect"; the binding CORE — signer, SHA-256/HMAC, DTOs, `ByteStream`, path/multipart/xml, and the cfg'd `R2BindingBackend` — being wasm-clean in isolation is the sufficient proof).
- The native build is green, the native build provably never links `worker` (target-cfg dependency, gated impl), and native `/r2` reuses `Backend::Http`.

**Proposal**: I endorse the Architect's recommendation — file the wasm composition as a t38 carry-over (a thin wasm Workers entrypoint crate composing the binding modules WITHOUT the tokio bridge, shared with t23's entrypoint), so the `--target wasm32` acceptance line is honored at the composition root rather than at the leaf. This is the same resolution the team accepted for t23; t22 should inherit it consistently, with the carry-over recorded with an owner rather than left as an unstated divergence.

I also note (informational, no action for me) the Architect's three recorded observations — Obs-1 versioning-aware irreversibility (doc-or-wire decision), Obs-2 SigV4 RFC-3986 URI/query encoding, and the `limit: true` over-claim — all live behind the t38-parked live HTTP path and none is reachable from my black-box E0 charter (no live S3/R2 call runs at E0). They are correctly out of E2E scope; my concern is only that they remain recorded carry-overs with owners, which the Architect already secured.

---

## Business-outcome assessment (Planner lens)

The universal-CRUD promise the ticket exists to prove is delivered and observable from the outside: an object is created with `UPSERT`, listed with `FROM`, fetched by reading the path, deleted with `REMOVE`, and addressed at a version with `@versionId` — all through one introspective `Driver` surface, with S3 and R2 sharing one core and differing only at the mount edge. The two stakeholder-critical safety properties — "never return wrong rows" (truthful residual) and "never leak a credential" — both survived active attempts to break them. The streaming/abort-on-error invariant (no orphan-part billing) is observable end-to-end. The single residual risk to the business outcome (full wasm-leaf build) is a known, consistently-handled park, not a surprise.

Status: under-review → **Approve (7/7 acceptance scenarios validated; one non-blocking wasm-park observation with a t38 carry-over proposal)**.
