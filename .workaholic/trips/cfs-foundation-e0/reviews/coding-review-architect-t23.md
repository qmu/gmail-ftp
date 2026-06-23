# Coding Review — Architect — t23 Cloudflare D1/KV/Queues driver (+ cfs-sql-core extraction)

- Reviewer: Architect (Neutral / structural bridge)
- Target: commit `327d41d` — `cfs-driver-cf` (mount `/cf`) + the extracted pure leaf `cfs-sql-core`
- Domain lens: structural integrity, translation fidelity, dependency-direction invariants, the headline D1 HTTP-path injection-safety carry-over from t17.
- Scope read: `crates/sql-core/src/{lib,error,emit,dialect,compile,catalog}.rs`; `crates/driver-cf/src/{lib,path,backend,effect,applier,schema,error,registry,tests}.rs`; `crates/driver-sql/src/{lib,error}.rs` (rewired) + its `tests.rs`; `crates/cmd/tests/dep_direction.rs`; `ARCHITECTURE.md`; `Cargo.toml` deltas. No tests executed (analytical review only).

## Decision: Approve with minor suggestions

The extraction is a clean pure leaf, both drivers reuse it while each stays an independent runtime leaf, t17's behavior is preserved, and the D1 HTTP path is injection-safe with a request-shape test that asserts it from both directions. The minor suggestions are coverage/fidelity hygiene, not defects: no mechanical pure-leaf test for `cfs-sql-core`, no co-located tests in the leaf itself, a hardcoded `"sql"` provenance reused by D1 `describe`, batch-atomicity scope wording, and un-encoded KV URL interpolation.

---

## 1. The cfs-sql-core extraction — clean pure leaf? (verdict: YES)

- **Pure leaf, cfs-types-only.** `crates/sql-core/Cargo.toml` workspace deps are `cfs-types` + `serde_json` + `thiserror`. No `reqwest`/`tokio`/`cfs-runtime`/`cfs-secrets`/`cfs-driver`. Every function in `emit`/`dialect`/`compile`/`catalog` is pure (no I/O, clock, RNG, runtime); `#![forbid(unsafe_code)]` is set. The runtime/secrets `From` adapters were deliberately left OUT (they live in the consuming driver) — error.rs docs this explicitly. This matches the `cfs-http-core` precedent exactly.
- **Both drivers reuse one emitter, each an independent runtime leaf.** `driver-sql` now `pub use cfs_sql_core::{...}` (re-exports for source compatibility); `driver-cf` depends on `cfs-sql-core` (the pure leaf) — NOT on `cfs-driver-sql` (a runtime leaf). The `ARCHITECTURE.md` spine entries are accurate: `cfs-sql-core → cfs-types` only; both drivers `→ … cfs-sql-core`. So neither runtime leaf depends on the other and tokio dead-ends in each — the durable structural goal is met.
- **Confinement test still composes.** `runtime_is_confined_to_plan_and_types` is the generic leaf-confinement test (b): any `cfs-runtime` consumer must be a leaf nothing depends back onto. `cfs-driver-cf` IS such a leaf (nothing depends on it), so the generic check admits it automatically; the only edit needed was the one-line *intent* allowlist append (b'), which the diff shows. Correct and minimal.
- **t17 behavior preserved.** `crates/driver-sql/src/tests.rs` still holds 22 `#[test]`/`#[tokio::test]` functions; the diff is import-rewiring (`crate::{dialect,emit,catalog}` → `cfs_sql_core::{...}`) plus swapping the `From<SecretError>`-based credential conversion for the `credential_error` free function. That swap is *forced and correct*: with `SqlError` now foreign to `driver-sql`, a `From<cfs_secrets::SecretError> for SqlError` impl would violate the orphan rule, so the explicit converter (`credential_error` / `sql_error_to_effect_error`) is the right relocation. The test was updated to call it. The orphan-rule relocation is clean.
- **Right durable structure.** Yes — same shape as `cfs-http-core`: a pure leaf single-sourcing logic two runtime leaves share, keeping the spine acyclic and tokio confined.

**Suggestion 1a (coverage, not a defect).** Unlike `cfs-http-core`, which has a dedicated `http_core_is_a_pure_leaf_single_sourcing_the_redaction_set` test in `dep_direction.rs` that *mechanically* forbids `reqwest`/`tokio`/`cfs-runtime` in the leaf and asserts both consumers depend on it, there is **no equivalent test for `cfs-sql-core`**. The purity invariant is prose-only. Recommend adding `sql_core_is_a_pure_leaf_single_sourcing_the_sqlite_emitter` mirroring the http-core test (assert sql-core's only workspace dep is `cfs-types`, forbid the runtime/vendor crates, assert both `cfs-driver-sql` and `cfs-driver-cf` depend on it). This makes the "neither runtime leaf depends on the other" guarantee enforced by construction for every future driver.

**Suggestion 1b (coverage).** `cfs-sql-core` carries **no co-located test module** — all emit/compile/dialect tests remained in `driver-sql/src/tests.rs` and exercise the leaf only through the re-export. The behavior is preserved, but the pure leaf has no test proving its contract independent of a consumer. Recommend migrating (or duplicating) the emitter/compiler golden tests into `crates/sql-core/src/` so the leaf is self-verifying.

## 2. D1 injection safety on the HTTP path (headline) — verdict: CONFIRMED SAFE

- **Every value is a bound param, never interpolated.** `emit.rs` `render_select`/`render_dml` place only quoted identifiers + `?` placeholders in the SQL string; values ride in `Vec<Param>`. `backend.rs::d1_query`/`d1_batch` build the JSON body `{"sql": sql, "params": [param_to_json(p)...]}` — `sql` is the rendered statement, `params` is the structured bound array. `param_to_json` maps each `Param` to a JSON scalar; a `Param::Text("'; DROP TABLE …")` becomes `Value::String`, inert. There is no code path that formats a value into `sql`.
- **The request-shape test actually asserts it (both directions).** `d1_select_pushes_where_and_binds_params_as_structured_array_not_interpolated` (tests.rs:148) parses to a `RecordedCall::D1Query { sql, params }` and asserts `!sql.contains("DROP TABLE")` AND `params == vec![Param::Text(INJECTION)]`. The write path test `d1_insert_lowers_to_parameterized_batch_with_bound_values` repeats the same two-sided assertion against `RecordedCall::D1Batch`. `RecordedCall::D1Query`/`D1Batch` carry the rendered `sql` + the `params` array precisely so the test can prove the value is in `params` and absent from `sql`. This is the exact assertion the t17 Architect carry-over asked for.

## 3. Batch atomicity — verdict: CORRECT, with a scope wording caveat

- `d1_batch` posts one `/batch` request = one atomic D1 transaction, correct given D1 has no interactive BEGIN/COMMIT. Each statement carries its own `params` array.
- **Partial-failure honesty.** `CfBackend::d1_batch` doc states the batch rolled back on any statement failure → `CfError`; `is_retryable` maps 5xx/429 + transport to retryable, everything else terminal; `is_irreversible` flags `QueueSend` (and the planner flags D1 destructive writes upstream) so the runtime never auto-retries an irreversible leg. Honest.

**Observation 3 (scope wording).** `applier.rs::apply_effect` for `D1Dml` sends **one effect node → one `d1_batch` of one statement**. A multi-statement D1 write spanning several effect nodes in a single COMMIT is therefore N independent atomic batches, not one transaction across all of them. The `end_to_end_commit_through_interpreter_for_all_three_services` test confirms this (3 effects → 3 separate backend calls). This is *consistent with t17's per-effect-node application model* and is fine, but several doc comments (lib.rs, ARCHITECTURE.md) read "one commit → one atomic transaction," which over-promises at the multi-effect granularity. Recommend tightening the wording to "one D1 write effect → one atomic `/batch`" so the atomicity boundary (per effect node, not per COMMIT) is unambiguous. No code change required.

## 4. KV + Queues correctness — verdict: SOUND

- **KV archetype/ops.** `BlobNamespace`; `kv_table_schema` is the degenerate `(key TEXT, value TEXT)` relation; caps split namespace (`ls,cp,mv,rm,select,upsert,remove`) vs key (`select,upsert,remove`). TTL rides as `?expiration_ttl=` on the put URL; metadata is carried on `KvEntry`. `kv_delete` treats 404 as success (idempotent). `kv_get` treats 404 as `None`. Round-trip test asserts TTL+metadata. Sound.
- **Queues.** `AppendLog`; caps `{insert,select}` only — `UPDATE`/`REMOVE`/`JOIN` denied at the parse gate (`update_on_a_queue_is_rejected_structurally`). `derive_idempotency_key` is a deterministic FNV-1a over the body (no RNG → purity-safe), so a retry of the same body yields the same key and de-dupes; `queue_send_derives_a_deterministic_idempotency_key_when_absent` asserts `keys[0]==keys[1]`. Sound for at-least-once de-dupe.
- **Per-service capabilities correct.** Write over a KV namespace is gated, UPDATE over a queue rejected — both have structural tests (`join_writes_over_kv_namespace_are_gated`, `update_on_a_queue_is_rejected_structurally`).

**Observation 4a (idempotency-key collision surface).** The FNV key is a function of the body ONLY. Two *intentionally distinct* messages with identical bodies derive the same key and would de-dupe against each other at Cloudflare. For a true event stream this is usually wrong (legitimate duplicate-content events get dropped). The explicit `idempotency_key` column is the escape hatch and is the right primitive; recommend a one-line doc note that the derived key is a *content* key (identical bodies are treated as the same message) so a caller who needs per-emission identity supplies an explicit key. Not a defect — the deterministic-when-absent behavior is the documented contract.

**Observation 4b (URL interpolation — correctness, NOT injection).** `kv_path`/`queue_path` and the KV key/prefix/limit are interpolated into the request URL un-encoded (`/values/{key}`, `prefix={p}`). A key/prefix containing `/`, `?`, `#`, or `&` would mis-route the path/query. This is a URL-correctness gap, orthogonal to SQL injection (which is fully closed). Recommend percent-encoding the path/query segments at request-build time. Low severity (keys are usually well-formed) but worth a follow-up.

## 5. Token safety — verdict: SAFE

- Cloudflare token is a `cfs_secrets::Secret`, exposed only in `HttpApiBackend::authed` into the `Authorization: Bearer` header; the `HttpRequest` `Debug` redacts it via the shared `cfs-http-core` authority. No `CfError` arm carries a token, header value, URL-with-token, or bound param value. `the_api_token_never_appears_in_any_error_surface` drives every `CfError` variant and asserts neither the token nor the `deadbeef` fragment leaks; `the_request_debug_redacts_the_bearer_token` asserts the bearer is replaced by `REDACTED`. HTTP is confined: the crate rides a LOCAL `HttpExchange` seam over `cfs-http-core` DTOs and does NOT depend on `cfs-driver-http`, so `reqwest` stays confined and the crate stays an independent runtime leaf.

## 6. Spine — verdict: ACYCLIC, allowlist composes, relocation clean

- `cfs-sql-core → cfs-types` (leaf). `cfs-driver-cf → {cfs-driver, cfs-plan, cfs-types, cfs-runtime, cfs-sql-core, cfs-http-core, cfs-secrets}` — a leaf runtime consumer nothing depends back onto, so the generic confinement check (b) admits it and only the one-line intent allowlist (b') needed editing. The orphan-rule From-converter relocation (`SqlError` adapters now explicit free functions in `driver-sql`) is correct and documented. `From<SqlError> for CfError` and `From<CfError> for EffectError` live in `driver-cf` (both halves local → orphan-rule-safe).

---

## Concern → proposal summary (one per Critical Review Policy, all minor)

1. **No mechanical pure-leaf test for `cfs-sql-core`** → add `sql_core_is_a_pure_leaf_single_sourcing_the_sqlite_emitter` to `dep_direction.rs` mirroring the http-core test (forbid runtime/vendor crates in the leaf; assert both drivers depend on it). *Highest-value follow-up — it converts the central extraction invariant from prose to a build-enforced guard.*
2. **Leaf has no co-located tests** → migrate/duplicate the emitter/compiler golden tests into `crates/sql-core/src/` so the pure leaf self-verifies.
3. **`describe_schema` hardcodes `DriverId::new("sql")`** in shared core, reused by D1 `describe` → a D1 column's provenance reports `driver: "sql"`, not `"cf"`. Parameterize the driver id (or override in the cf describe) so federated-JOIN provenance is faithful.
4. **Batch-atomicity wording** over-promises ("one commit → one atomic transaction") → tighten to "one D1 write effect → one atomic `/batch`."
5. **Idempotency key is content-only** → doc that identical bodies de-dupe; explicit key is the per-emission escape hatch.
6. **KV URL interpolation un-encoded** (correctness, not injection) → percent-encode path/query segments.

None blocks acceptance. The two headline asks — a clean `cfs-sql-core` pure-leaf extraction that both drivers reuse while staying independent runtime leaves, and an injection-safe D1 HTTP path with a request-shape test that asserts the structured-bound-array invariant from both directions — are both met.
