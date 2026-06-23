# Architect Analytical Review — t24 GitHub Driver

- Reviewer: Architect (Neutral / structural bridge, translation fidelity)
- Artifact reviewed: commit `43008e9` — `crates/driver-github/` (new), `cfs-http-core` (`HttpMethod::Patch`), `cfs-driver-gdrive` (PUT→PATCH), `crates/cmd/tests/dep_direction.rs` (allowlist append)
- Method: analytical / code + model review only (no build, no test, no clippy — per role)
- Decision: **Approve with minor suggestions**

---

## 1. PRIMARY RULING — http-seam reuse vs leaf-confinement

**Ruling: the Constructor's resolution is structurally CORRECT. This is faithful translation of the ticket's intent, not a divergence. No revision warranted on this axis.**

### The adjudication

The ticket says "reuse the existing reusable HTTP client seam from `cfs-driver-http` (t18)". The Constructor read this as a *value* requirement (no second HTTP DTO, single redaction authority, the Bearer/Link/429 seam shape) rather than a *literal crate-edge* requirement (`cfs-driver-github → cfs-driver-http`). I checked whether the literal reading is even admissible, and it is not:

- `crates/cmd/tests/dep_direction.rs` encodes the leaf-confinement invariant in two layers. Layer (b) is the **generic** check: every `cfs-runtime` consumer (other than the runtime itself) MUST be a leaf — no workspace crate may depend back onto it. `cfs-driver-http` is itself a runtime consumer (it appears in `runtime_consumers_allowed`). If `cfs-driver-github` took a crate dependency on `cfs-driver-http`, then `cfs-driver-http` would have a crate depending back onto it, making it a **non-leaf runtime consumer** — and layer (b) fires automatically with "tokio could transit out of the runtime, through {consumer}, and back into the spine."

- So the literal reading is *self-contradictory*: you cannot both keep t18 a confined runtime leaf and let t24 import it. The ticket's two constraints ("reuse t18's seam" + the inherited dep-direction invariant from t13/t18) can only both hold if "reuse" means DTO-level + shape-level reuse, which is exactly what was built.

The token-leak intent behind "reuse t18" is the t19 lesson: do not hand-roll a second `HttpRequest`/`HttpMethod`/`SENSITIVE_HEADERS` that can drift and leak. That intent is **fully satisfied** — `client.rs` imports `cfs_http_core::{HttpMethod, HttpRequest, HttpResponse}` and never defines its own; redaction is the single `cfs-http-core` authority. `Cargo.toml` confirms `cfs-http-core` is the only HTTP dep and there is no `cfs-driver-http` edge. The structural twin to gdrive's `HttpExchange` over `cfs-google-auth` is exact and already-blessed precedent in this codebase.

Verdict: **leaf-confinement wins; DTO-level reuse satisfies the token-leak intent.** Translation fidelity is preserved — the ticket's *purpose* is met, and the one literal phrase it could not honour was un-honourable under an invariant the same ticket inherits. The `Cargo.toml` description and the `lib.rs`/`client.rs` module docs document the reasoning in-place, which is the right place for a future reader to find it.

### The structural concern I must raise (consolidation debt)

There are now **three structural twins** trading the same `cfs-http-core` DTOs over a synchronous send-seam:
- t18 `cfs-driver-http::HttpClient` (+ `ReqwestClient`/`MockHttpClient`)
- `cfs-google-auth::HttpExchange` (+ `MockExchange`)
- t24 `cfs-driver-github::HttpTransport` (+ reqwest impl parked / `RecordingTransport` in tests)

Each is a `Send + Sync` trait with a single `fn send(&HttpRequest) -> Result<HttpResponse, _>` and a local secret-free transport-error type. This is *acceptable* reuse today (the DTOs and redaction are shared — the leak hazard is closed), but the **seam trait itself** is now triplicated, and each new REST-shaped driver that cannot take a runtime-leaf dep on t18 will mint a fourth, fifth. That is an emerging consolidation debt, not a present defect.

**Constructive proposal (carry-over, not a blocker):** extract the transport *trait* (and its secret-free `TransportError`) into `cfs-http-core` as e.g. `pub trait HttpTransport` — a pure-leaf trait over the DTOs it already owns. `cfs-http-core` has no reqwest/tokio, so the trait stays pure; each driver keeps its own concrete wire impl but stops re-declaring the seam shape. This collapses three twins into one shared seam *without* re-introducing the leaf-confinement violation (the trait lives in the pure leaf, not in t18). I recommend filing this as a carry-over (a "t-consolidate-http-seam" follow-up) rather than blocking t24 — retrofitting it now would touch three drivers mid-trip.

---

## 2. Pushdown residual truthfulness (highest-value correctness check) — PASS

`pushdown.rs` is the t20 discipline applied correctly:
- `state = 'open'|'closed'` and `assignee = '<login>'` → `Lowered::Exact`: pushed AND dropped from residual. GitHub's `state`/`assignee` params are exact equality, so dropping the conjunct is sound.
- `label`/`labels = '<name>'` → `Lowered::PreFilter`: pushed as a narrowing param BUT the original predicate is **kept** in the residual (`Some(p.clone())`). GitHub `labels` is set-membership (the issue may carry other labels too), so a scalar `=` must be re-checked locally. Over-fetch then filter — never wrong rows.
- `And(a,b)` recurses and combines residuals correctly: an exact conjunct drops to `None`, a lossy conjunct survives, and `(Some, Some)` reconstructs the `And`. The four residual-merge cases are exhaustive and correct.
- `Or`/`Not`/`In`/`Between`/`Like`/dotted-column/non-`Text` literals all fall to `Some(p.clone())` — wholly residual. **No silent row-drop path exists**: the only place a predicate leaves the residual is the `Exact` arm, which is gated on param-≡-predicate semantics.

`read.rs::ReadPlan::list` further confines pushdown to `Issues`/`Pulls` (the only namespaces with these list params); every other namespace gets empty params + the whole predicate as residual. Correct and conservative. Tests `where_state_and_label_push_to_params_with_label_kept_residual`, `exact_predicates_push_fully_with_no_residual`, and `or_predicate_stays_wholly_residual` pin exactly these invariants.

**Minor observation:** `state = 'merged'` (a pseudo-state GitHub does not accept on the `state` param — it only takes `open`/`closed`/`all`) would push `state=merged` as Exact and drop the residual; GitHub would 422 or return nothing. This is a value-domain question one layer up (the schema/validation of the `state` column), not a pushdown-shape defect — the mapping is honest for the values GitHub's param accepts. Worth a one-line note in a follow-up that pushdown trusts the column's value domain. Not a blocker.

---

## 3. Token safety — PASS

End-to-end no-leak path is sound:
- PAT is a `cfs_secrets::Secret`, resolved only in `RestGitHubClient::request()` at request-build time via `secrets.get(&cred)`, exposed via `expose_str()` into a single `format!("Bearer {token}")` header, then dropped. Never stored on the struct (`RestGitHubClient` holds `Arc<dyn Secrets>` + `CredentialKey`, not the token).
- The header rides in `HttpRequest`, whose **manual redacting `Debug`** (verified in `http-core/src/lib.rs`) replaces every `is_sensitive_header` value (incl. `authorization`) with `REDACTED`. The structured request log (`send_one`/`send_get`) emits only `method` + `url` + `status` — never headers.
- `GitHubError` (error.rs) carries only op/status/path/code/class-reason — no header value, no token. The `From<TransportError>` mapping crosses only the secret-free `reason` class string.
- Tests prove it: `errors_are_secret_free` (no `Bearer`/`ghp_`/`token`), `planted_token_never_appears_in_a_serialized_plan` (canary `ghp_PLANTED…` absent from serialized plan + preview), `rest_client_injects_bearer_pat_and_never_logs_it` (Bearer on the wire, redacted in `Debug`).

No leak path found.

---

## 4. `HttpMethod::Patch` addition + gdrive switch — PASS, non-regressive

- The enum is `#[non_exhaustive]` (unchanged); adding `Patch` is additive and downstream matches must already have a wildcard or be in-crate. `as_str`/`Display` cover it; the doc reconciles the prior 2-vs-4 drift the t19 note describes.
- `is_retry_safe()` returns `!matches!(self, Post | Patch)` — **PATCH is correctly classified not-retry-safe** (RFC 7231: PATCH is not guaranteed idempotent; a timed-out PATCH may have applied). This is coherent with the runtime's never-retry-non-idempotent policy. The unit test `methods_render_uppercase_tokens_and_post_is_not_retry_safe` asserts `!Patch.is_retry_safe()`.
- gdrive switch: `modify_file` (metadata rename/re-parent) and `trash` (`trashed=true`) move PUT→PATCH. This is *more* correct on the wire — Google Drive `files.update` is genuinely PATCH-semantics partial update; the prior PUT was the drift artifact. Critically, gdrive's `send()` (client.rs:149) does **no method-based retry at all** — it is a single-shot send that errors on non-2xx — so there is no retry-classification regression from the verb change. `update_content` correctly stays PUT (media replace-by-id is the idempotent upsert path, retry-safe). No gdrive test asserts on `Put` for these two ops (grep clean), so nothing breaks.
- github applier (`applier.rs::method_of`) maps `PatchIssue`/`PatchPull` → `Patch`; the `apply_shared` retry gate is `is_retry_safe() && !is_at_least_once_post() && !Merge` — PATCH falls out via `is_retry_safe()==false`, so a transient PATCH failure is reported terminal. Coherent with the at-least-once / never-retry-non-idempotent contract.

---

## 5. Capability gating at parse time — PASS

`GitHubDriver::caps_for` is node-keyed off `effective_namespace()`: `issues|pulls → S/I/U`; `comments|releases|branches → S/I/R`; `reviews|runs|files → S`; repo-root/unknown → `none()`. This is the introspective `Driver::capabilities` surface the planner's `check_capability` consults **before** an effect is built, so `UPDATE /github/o/r/runs/…` is rejected structurally, not at apply time. Test `update_on_runs_is_rejected_at_parse_time_with_structured_error` confirms the structured `CapabilityDenied`. The apply-leg `cap_denied` in `effect.rs` is the belt-and-suspenders twin (a defense-in-depth terminal error if a mis-routed effect ever reaches decode) — correct layering.

---

## 6. Irreversibility / merge concurrency / dispatch honesty — PASS

- `is_irreversible()` covers the three deletes + `Merge` + `Dispatch`; `review` is reversible (a later review supersedes). `procs.rs` marks merge/dispatch `irreversible(true)`, review not. Test `procedures_are_declared_with_irreversibility_and_scopes` + `preview_of_a_merge_plan_surfaces_irreversible_and_performs_no_io` confirm PREVIEW surfaces the irreversible flag with zero I/O (mock never called).
- Merge optimistic concurrency: `sha` is an optional precondition sent only when supplied (`if let Some(s) = sha`), so GitHub 409s on a stale ref. `apply_shared` additionally forces merge terminal-on-transient (`!matches!(.., Merge)`) so an irreversible merge is never auto-retried even though PUT is wire-idempotent — exactly right.
- Dispatch 204/no-run-id: modelled as a queued resolution (`Ok(1)`, doc says a follow-up `SELECT … FROM runs` resolves the id) — **no fabricated id**. Honest.

---

## 7. No vendor/SDK leak — PASS

`dto.rs` owns `IssueDto…FileMetaDto` with `serde`; `read.rs` decodes GitHub JSON into them via field accessors and `From<&Dto> for Row`. `serde_json::Value` is used only *inside* the `client`/`read` boundary (list body, effect payloads) and never crosses the `Driver`/`Plan` public surface. The `GitHubClient::list` returning `serde_json::Value` is an internal-trait return, not a `Driver` public signature. No octocrab/reqwest type appears in any public sig. The DTO-projection test (`dtos_project_onto_their_schema_in_column_order`) pins the row shape.

---

## 8. `files`/`branches` boundary vs the t26 git driver — PASS, documented

`path.rs` and `lib.rs` both document the boundary explicitly: `files` is a **read-only API content-metadata view** (path/sha/size/type — `decode_files`), `branches` is **ref metadata** (read + create-ref/delete-ref). Capabilities enforce it: `files → SELECT` only; `branches → S/I/R` where I/R are ref create/delete (`CreateBranch` POSTs `git/refs`, `DeleteBranch` DELETEs `git/refs/heads/…`). No blob content, no commit history, no mutable-refs walk — those are explicitly deferred to t26 in the module doc. The boundary is stated where t26's author will read it. Good fence.

---

## 9. Dep direction / leaf confinement — PASS

`cfs-driver-github` appears in `runtime_consumers_allowed` (one-line reviewable append) and is a genuine leaf: nothing in the workspace depends back onto it, so layer (b) admits it. Its dep closure is `cfs-driver`, `cfs-plan`, `cfs-types`, `cfs-runtime`, `cfs-secrets`, `cfs-http-core` + serde/thiserror/tracing — no spine inversion, no `cfs-driver-http` edge. Clean.

---

## Additional minor observations (non-blocking)

1. **Branch names with slashes.** `DeleteBranch`/`CreateBranch` take the ref from a single path segment via `object_id()`. A branch named `feature/x` would be parsed by `path.rs` as `id=feature, sub=x` → `x` is not a known namespace → `InvalidPath`. So slashed branch names are unaddressable through the path form today. This is an honest *limitation* (it errors, never mis-targets), but worth a doc note or a future encode of multi-segment refs. Proposal: document "branch refs addressed by path are single-segment; slashed refs are a t26/follow-up concern," or accept a trailing-segments join for the `branches` namespace specifically.

2. **`send_get` reads `Retry-After` then discards it** (`let _retry_after = …`). The comment is honest ("the bound is the retry budget itself"), but a malicious/huge `Retry-After` is correctly ignored rather than slept-on — which is the safe choice for a synchronous seam. No change needed; just confirming this is intentional and safe, not a dropped feature.

3. **`Prom 5` / pushdown value-domain** (see §2 minor): pushdown trusts the `state` column's value domain. A one-line follow-up note that exact-pushed params assume the column value is in GitHub's accepted set would close the last truthfulness corner.

---

## Decision

**Approve with minor suggestions.**

The primary structural question is resolved correctly: DTO-level + seam-shape reuse over `cfs-http-core` is the faithful translation of "reuse t18," because the literal crate-edge reading is forbidden by the same leaf-confinement invariant the ticket inherits. Token safety, pushdown residual truthfulness, capability gating, irreversibility, the PATCH addition + gdrive switch, the no-vendor-leak boundary, and the files/branches fence against t26 are all sound and well-tested.

The one structural debt worth recording is the **three-way seam-trait twin** (t18 `HttpClient` / gdrive `HttpExchange` / t24 `HttpTransport`): acceptable today, but I recommend a carry-over to hoist a shared `HttpTransport` trait into the pure `cfs-http-core` leaf so the fourth REST driver does not mint a fourth twin. The minor items (slashed branch refs, pushdown value-domain note) are documentation-level and can ride the same follow-up.
