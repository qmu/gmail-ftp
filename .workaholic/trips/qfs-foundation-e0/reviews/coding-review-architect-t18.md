# Coding Review (Architect) — t18 Generic HTTP/REST driver + http.get TVF

- **Reviewer**: Architect (Neutral / structural bridge)
- **Commit**: `5cc1e84` on `work-20260622-230954`
- **Scope**: analytical review only (no cargo/test execution) — `crates/driver-http/src/{lib,applier,client,config,effect,error,request}.rs`, `tests.rs`, `tests/wire.rs`, `crates/cmd/tests/dep_direction.rs`, `crates/driver-http/Cargo.toml`, `crates/secrets/src/secret.rs`, `ARCHITECTURE.md`.
- **Decision**: **Approve with minor suggestions**

---

## Headline verdict — token safety

**It is structurally impossible for a live token to reach a Debug, log, or error surface.** The leak surface was enumerated and each path closed:

1. **The single read door.** `grep "expose"` across `crates/driver-http/` returns exactly one read of live material: `applier.rs:134` `secret.expose_str()`. It is invoked at request-build time inside `inject_auth`, the returned `&str` is immediately `format!`-ed into a header *value* and the borrow ends — it is never passed to `format!`/`tracing`/an error constructor. The other matches in the crate are doc comments. This is the grep-able single-door discipline the secrets crate advertises (`secret.rs:54-60`), upheld here.

2. **Config holds no token (SecretRef indirection is sound).** `config.rs` carries only a `SecretRef { driver, account }` selector; `AuthStrategy` has no variant that can hold key material. `RestApiConfig` derives `Serialize`/`Debug` and is provably safe to log — `config_round_trips_through_serde_without_any_token` asserts the planted token never appears in the serialized form. The token is resolved from the injected `Secrets` handle only at apply time (`inject_auth`), never at plan-build time.

3. **The redacting Debug covers the live-token surface.** Once resolved, the token sits as a plain `String` in `HttpRequest.headers`. `HttpRequest`'s **manual** `Debug` (`request.rs:144-165`) redacts the value of every header whose name matches `SENSITIVE_HEADERS` (case-insensitive), emitting `qfs_secrets::REDACTED` in its place and the body *length* only. The `SENSITIVE_HEADERS` list (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`, `api-key`, `x-auth-token`) covers Bearer (`Authorization`) and the common custom-header keys. `HttpResponse` gets the same redacting Debug (covers `Set-Cookie`). `the_auth_token_never_appears_in_debug_or_log_surfaces` is a direct, planted-token assertion.

4. **No token in the structured log or any error.** `send_one` (`applier.rs:148-153`) logs only `method`, `url`, `status` as explicit scalar fields — never the header vec, never `{:?}` of the request. Every `HttpError` variant (`error.rs`) is constructed from request *shape* (method/URL/status/code) — there is no field that can carry a header value, and `from_status` / `transport_reason` interpolate only the class and the URL. A 401/403 deliberately surfaces "client error {status} for {method} {url}" with no token (`client_4xx_is_terminal_and_carries_no_token`). A missing credential surfaces a secret-free code, before any send (`missing_credential_is_a_structured_auth_error_not_a_panic`).

**Token-safety verdict: PASS.** No reachable path leaks a token via Debug, log, or error. The redaction is enforced at the DTO boundary (the only `Debug` a request has), so it cannot be bypassed by an enclosing `{:?}` dump.

### Token-safety observations (non-blocking)

- **O1 — plaintext lifetime widens past the `Secret`.** Once `expose_str` copies the token into the header `String`, that copy is an ordinary heap `String` (not `Zeroizing`) and is **cloned** into each pagination follow-up request (`rebuild_with_url` → `current.headers.clone()`) and into the recorded mock request. This is unavoidable (it must reach the wire) and never *logged*, so it is not a leak — but the in-memory plaintext copy has a wider, non-zeroized lifetime than the `Secret` it came from, and is duplicated per page. Acceptable for now; if E5 hardens memory hygiene, a `Zeroizing<String>` header-value or a redact-on-drop wrapper for the resolved value would shrink the window. Worth a one-line note in the secrets-hygiene backlog, not a t18 change.
- **O2 — `is_sensitive_header` is an exact-name allowlist.** A non-standard auth header name (e.g. `X-Gitlab-Token`, `X-Figma-Token`) would *not* be redacted. t24/t25 use `Authorization`/`X-Api-Key` (both covered), so this is not a present risk, but the seam should make redaction follow config: when `AuthStrategy::Header { name, .. }` injects a custom header, that `name` is known to be sensitive and could be registered with the redactor. Suggest (non-blocking) either (a) documenting that any new auth header name be added to `SENSITIVE_HEADERS`, or (b) having `inject_auth` thread the configured header name into a per-request sensitive-set. Today's matrix is safe; this is a forward-guard for the next driver that uses a vendor-specific header name.

---

## Reusability for t24 (GitHub) / t25 (Slack)

**The REST seam is genuinely API-agnostic and t24/t25 can layer on without forking it — with ONE missing extension point that t25 (Slack) needs.**

What is reusable as-is: `HttpRequest`/`HttpResponse`/`HttpMethod` owned DTOs, the `HttpClient` trait + `ReqwestClient`, `inject_auth` (Bearer + custom-header), status→`HttpError` classification, codec decode, and **both** pagination strategies a real API needs. GitHub maps cleanly: Bearer auth, `LinkHeader { max_pages }` is exactly GitHub's RFC-5988 `Link; rel="next"` (verified by `link_header_pagination_follows_rel_next`), and 429/5xx→`Server`(retryable) covers rate-limit backoff at the class level. A GitHub driver supplies a `RestApiConfig` and reuses everything — no HTTP path re-implementation. **GitHub: reuses without forking.**

Slack maps for auth (Bearer) and pagination (`Cursor { next_field: "response_metadata.next_cursor"-ish, param: "cursor", max_pages }` — verified by the cursor tests, which stop when the cursor is absent and enforce the cap). **But Slack's signature failure mode is HTTP 200 + `{"ok": false, "error": "..."}` in the body.** The current seam classifies success purely on `HttpResponse::is_success()` (the 2xx status). A Slack `ok:false` would be treated as a successful decode, the error swallowed into a "row", and — worse — `cursor_from_body` could mis-follow. There is **no body-based error-detection extension point** today.

This is **not a fork** (Slack still reuses request/auth/pagination/decode), but it **is a missing hook** Slack will have to add. Two non-blocking options for t25, the cleaner one being a seam addition now:

- **Proposal R1 (recommended, small):** add an optional `BodyErrorRule` to `RestApiConfig` (closed/`None` by default) — e.g. `{ ok_field: "ok", expect: true, error_field: "error" }` — checked in `send_one`/`decode` after a 2xx, mapping a body-level failure onto a new `HttpError::Body { code }` (secret-free, terminal). This keeps the GitHub path untouched (rule defaults off) and gives Slack a config-only path with zero seam fork. Add the variant now even if t25 wires it, so the seam shape is fixed before two consumers depend on it.
- **Proposal R2 (defer):** let t25 detect `ok:false` in its own thin wrapper over `apply_effect`. Workable but it means Slack reaches *around* the decode/paginate path rather than *through* it, re-introducing exactly the per-API HTTP logic the seam exists to prevent. Prefer R1.

I would not block t18 for this — the seam is correct for the verbs it scopes and GitHub needs nothing more — but flag R1 as the one extension point to add before/with t25 so Slack does not bolt body-error logic outside the seam.

---

## Confinement / spine

- **reqwest/tokio confined to this leaf.** `Cargo.toml` pulls `reqwest` (rustls, no system OpenSSL) and `tokio` (`rt`, `macros`) only into `qfs-driver-http`. No `reqwest`/`url` type crosses the `HttpClient` trait or appears in any public signature (`lib.rs` re-exports only owned DTOs). `client.rs` is the sole reqwest site; `request.rs` doc asserts the boundary.
- **Generic leaf-confinement rule composes.** `dep_direction.rs::runtime_is_confined_to_plan_and_types` is the t16 generic rule: part (b) admits **any** leaf runtime consumer with no per-driver edit, and `qfs-driver-http` satisfies it (nothing depends back onto it). The named identity allowlist (b') was appended (`runtime_consumers_allowed = ["qfs-driver-local", "qfs-driver-http", "qfs"]`) — a one-line, reviewable signal exactly as the rule intends. The generic check (b) guarantees the append is *safe*; the allowlist pins *intent*. Composition is correct and the test stays green by construction.
- **`block_on` on a dedicated current-thread runtime is sound, not a fragility.** `ReqwestClient` owns its runtime for its whole lifetime; `send` calls `rt.block_on` on the bridge's `spawn_blocking` thread, which has **no enclosing runtime entered**, so there is no nested-runtime panic hazard. The per-request timeout (default 30s) bounds a hung endpoint to a transport error rather than a wedged thread. Construction is **panic-free** (lib policy): a failed client/runtime build degrades to `rt = None`, and every `send` then returns a structured `HttpError::Transport` rather than panicking (`client.rs:103-107`). Drop holds the runtime in an `Option` and calls `shutdown_background()` (non-blocking), avoiding the "cannot drop a runtime in an async context" panic when dropped under `#[tokio::test]`. This is the correct, defensive shape.
  - **O3 (minor):** `ReqwestClient::new` builds one current-thread runtime per client. The wire test drives async reqwest *inside* a `#[tokio::test(multi_thread)]` — `block_on` on a current-thread runtime nested under the test's multi-thread runtime works because `send` is reached via the bridge's blocking thread in the real path; in the wire test it is reached via the interpreter `commit`, also offloaded. This is fine, but a one-line comment in the wire test noting *why* the nested runtimes do not deadlock (the blocking-thread offload) would help the next reader. Non-blocking.

---

## Contract fit

- **Verb→method mapping faithful.** `effect.rs::from_node`: `Read|List→GET`, `Insert→POST`, `Upsert→PUT`, `Remove→DELETE`; `Update`(PATCH) and `Call` are terminal decode errors (out of scope per ticket). Matches RFD §3. `REMOVE` carries `irreversible` from the node. Verified by `insert_builds_post_upsert_put_remove_delete_with_irreversible` and `update_and_call_are_terminal_decode_failures`.
- **`http.get` TVF correct.** `http_get_node`/`http_get_args` carry an absolute override URL (`__http_url`) + override headers (`__http_h:<name>`); `apply_effect` routes an override-URL GET to `send_one` (single exchange, **no pagination** — correct for a no-config probe), uses the URL verbatim (not joined to a config base), and decodes via the codec. `http_get_tvf_issues_a_no_config_get_and_decodes_rows` confirms the override URL bypasses the config base.
- **Pagination bounded (no runaway).** `send_paginated` iterates `0..cap` where `cap = max_pages().max(1)`; `Pagination::None` → cap 1. The loop cannot exceed `max_pages` even if every page advertises a next cursor (`cursor_pagination_stops_at_the_page_cap` asserts exactly `max_pages` requests). The plan stays a single pure `HttpEffect`; the follow loop lives at the apply edge — faithful to the ticket's "genuinely hard part" resolution.
- **POST-never-retried (idempotency) correct.** `HttpMethod::is_retry_safe()` returns false only for POST; `into_effect_error(method_retry_safe)` downgrades a transient (`Transport`/`Server`) to **terminal** when the method is not retry-safe, so the interpreter never re-sends a non-idempotent create. `server_5xx_on_a_post_is_terminal_never_retried` and `server_5xx_on_a_get_is_retryable` pin both sides. PUT/DELETE stay retry-safe (idempotent on the wire). Correct per RFD §6.
- **JSON-as-open-column is the right call.** `describe` returns `RelationalTable` + a single open `value: Json` column rather than inventing typed columns — honest for weakly-typed REST JSON (RFD §4), and it does not over-commit the seam to a schema the typed-schema story (a later, per-API concern) can refine. The typed-schema path is correctly *not* forced here; a specific API (t24/t25) can layer a typed `describe` over the same machinery later. Right structural call.
- **O4 (minor) — affected-row honesty.** `apply_effect` returns the decoded row count as `affected`. For a write (`POST`/`PUT`/`DELETE`) the response body row count is the *response* shape, not strictly "rows affected" — e.g. a 204 DELETE with an empty body decodes to 0 even though one object was removed. The ledger then records `affected: 0` for a successful delete. Not wrong (it is an honest decode-count), and `affected_estimate_is_honest_for_a_filtered_get` shows the node can carry `Affected::Unknown`, but a short doc note on `apply_effect` that "affected = decoded response rows, which for a bodyless write may be 0" would prevent a future reader treating it as a mutation count. Non-blocking.

---

## Honesty of the parks

All deferrals are honest and non-blocking:
- **DSL body lowering = evaluator (E1) job** — `read_body` reads a pre-encoded `__http_body`; the applier does not invent body encoding. Honest seam: the evaluator owns row→body lowering. Today's tests pre-encode the body, which is the correct boundary.
- **WHERE→query pushdown = E3** — `PushdownProfile::Partial { limit: true, .. }` declares *only* the pagination-limit it actually pushes today; `set_query_param`/`percent_encode` are a documented thin passthrough, not a full predicate lowering. The driver does not over-claim pushdown. Honest.
- **OAuth acquisition/refresh = E5** — t18 *consumes* a resolved `Secret` via `Secrets`; it neither stores nor refreshes. The `SecretRef` indirection is exactly the consumer-side seam E5 will fill. Honest and correctly scoped.
- **wasm `fetch` shim** — named in the ticket/docs as a feature-gated future impl behind the same `HttpClient` trait; the trait is the seam, so the shim is a drop-in. The current crate does not ship it but the boundary is ready. Honest.

---

## Coherence / doc-catchup

- **D1 (non-blocking) — ARCHITECTURE.md lags the code.** The crate table and the tokio-confinement note (b') in `ARCHITECTURE.md` still enumerate only `qfs-driver-local` / `qfs` as runtime-leaf consumers; `qfs-driver-http` is in the *test* allowlist but not yet in the ARCHITECTURE crate table or the (b') prose, and there is no `/rest` / `http.get` row. The t16 review established the precedent that ARCHITECTURE catches up to each landed driver. Suggest a follow-up ARCHITECTURE edit adding the `driver-http` crate row + dep edge and updating the (b') leaf list to include `qfs-driver-http`. Documentation only; the mechanical test is already correct.

---

## Summary

The token-safety story is airtight: one grep-able `expose_str` door, a `SecretRef`-only config, a manual redacting `Debug` at the DTO boundary, and shape-only errors/logs — no reachable leak. The seam is genuinely API-agnostic; GitHub reuses it without a fork, and Slack reuses it for auth/pagination/decode but needs **one** missing hook (body-level `ok:false` error detection, proposal R1) which I recommend adding to `RestApiConfig` before t25 so Slack does not bolt error logic outside the seam. Confinement composes with the generic t16 rule via a clean one-line allowlist append; the dedicated-runtime `block_on` is sound and panic-free. Contract fit (verb mapping, TVF, bounded pagination, POST-never-retried, JSON-open-column) is faithful, and the parks are honest.

**Decision: Approve with minor suggestions.** No defect, no forced fork. Carry forward (none blocking): R1 (body-error hook for t25 — add the config variant now), O2 (custom-auth-header redaction guard), O1 (resolved-token memory hygiene → E5 backlog), O4 (affected-count doc note), D1 (ARCHITECTURE catch-up).
