# Coding Review — Architect — t19 (Google OAuth + multi-account auth base)

- **Reviewer**: Architect (Neutral / structural)
- **Commit**: `c9d1ae8` — `[Constructor] Implement t19 Google OAuth + multi-account auth base`
- **Crate**: `qfs-google-auth`
- **Scope reviewed**: `src/{lib,oauth,token,source,client,http,authorize,error}.rs`, `Cargo.toml`,
  `crates/secrets/src/{secret,key}.rs` (Secret/AccountId guarantees), `crates/driver-http/src/{client,request}.rs`
  (the t18 shape this seam mirrors), `crates/cmd/tests/dep_direction.rs` (confinement test), `ARCHITECTURE.md`.
- **Method**: analytical review only — no cargo/test execution (Architect QA domain).

## Decision: **Approve with minor suggestions**

Token safety is sound and the two-seam decision is the correct call for this commit. The suggestions
below are durability hardening for the *three* drivers that will build on this (t20/t21/t41), not defects.
None rises to a token-leak or correctness fault, so this is not a revision request.

---

## 1. Token safety (headline) — PASS

The secret discipline is rigorous and verified structurally end to end:

- **All four secret classes are `Secret` or wire-only.** `client_secret`, the refresh token, and the access
  token are `qfs_secrets::Secret` (no `Clone`, no `Serialize`/`Deserialize`, redacting `Debug`/`Display`,
  zeroized on drop — confirmed in `crates/secrets/src/secret.rs`). The authorization `code` is a borrowed
  `&str` that moves straight onto the form body and is never stored in a field.
- **`expose` only at the HTTP-inject point.** Greppable doors are exactly: `client_secret.expose_str()` in
  `OAuthClient::encode_token_form` (form body), `refresh_token.expose_str()` in `refresh_access_token` (form
  body), and `AccessToken::bearer()` (= `value.expose_str()`) in `client.rs::with_bearer` and
  `oauth.rs::fetch_profile_email` — all at request-build time, none into a logged field. Good.
- **Redacting `Debug` on every token-bearing DTO.** `AccessToken`, `GoogleAccount`, `BorrowedToken`,
  `HttpRequest`, `HttpResponse` all have manual `Debug` that delegate the secret field to `Secret`'s
  redaction or substitute `qfs_secrets::REDACTED` for sensitive header *values* (shared `SENSITIVE_HEADERS`
  gate mirroring t18). `TokenResponse`/`Userinfo`/`OAuthErrorBody` derive `Debug` but are internal and never
  logged — and `OAuthErrorBody` deliberately drops `error_description` (which can echo request detail),
  reading only the `error` slug. That is the right call.
- **Errors are secret-free by construction.** `AuthError` variants carry only `&'static str` endpoint/code,
  status numbers, OAuth slugs, account emails (low-sensitivity), and fixed reasons. The `From<SecretError>`
  preserves only `err.code()`. No variant can hold a token. The redaction canary test
  (`no_secret_appears_in_any_error_or_dto_surface`) plants the secret *as the client_secret* and drives every
  error Display+Debug and DTO Debug — the strongest form of this assertion.
- **No auth-URL leak path.** `build_auth_url` appends only `client_id`, `redirect_uri`, `response_type`,
  `scope`, `access_type`, `prompt`, `state` — the `client_secret` is **never** in the auth URL (correct;
  it rides only the token-exchange POST body). The token POST sets `client_secret` in the form *body*, and
  `post_token`/`refresh` log only the endpoint + status, never the body. The `HttpRequest` Debug reports
  `body_len`, never body bytes. There is no token-exchange request-log leak.

**Verdict: no leak path found on any surface.** This meets the RFD §10 headline invariant.

One small observation (non-blocking): `fetch_profile_email` builds `format!("Bearer {bearer}")` directly,
and `with_bearer` does the same. Both land in an `Authorization` header that the redacting `Debug` covers, so
there is no leak — but the bearer string materializes as a plain `String` on the stack (not a `Secret`)
between `expose_str()` and header insertion. This is unavoidable at the wire boundary and matches t18, so it
is acceptable; just noting it is the one place a live token exists as plain `String`, and it is correctly
short-lived and never logged.

## 2. The two-seam decision — CORRECT for now; recommend extracting `qfs-http-core` before t20/t21/t41 land

**The call to NOT depend on `qfs-driver-http` is right.** `dep_direction.rs::runtime_is_confined_to_plan_and_types`
mechanically enforces that every `qfs-runtime` consumer is a leaf (test (b)) and pins the allowlist to
`{qfs-driver-local, qfs-driver-http, qfs}` (test (b')). `qfs-driver-http` depends on `qfs-runtime`; if
`qfs-google-auth` depended on `qfs-driver-http`, the latter would stop being a leaf and test (b) would fire.
Keeping `qfs-google-auth → qfs-secrets` only (secrets → types) keeps it entirely off the runtime and off the
spine. The local synchronous `HttpExchange` is the structurally honest way to stay leaf-clean. **Approve the
confinement reasoning.**

**But the local seam is a genuine duplicate of t18's pure DTO layer, and it has already drifted.** I diffed
the two shapes:

| Element | t18 `qfs-driver-http` | t19 `qfs-google-auth` | Drift |
| --- | --- | --- | --- |
| `HttpMethod` | `Get/Post/Put/Delete`, `#[non_exhaustive]` | `Get/Post` only, **not** `#[non_exhaustive]` | Diverged closed set |
| `SENSITIVE_HEADERS` | 7 entries | 7 entries (copied verbatim) | Duplicated literal — drifts silently if t18 adds one |
| `is_sensitive_header` | identical | identical | Duplicated logic |
| `HttpRequest`/`HttpResponse` | redacting `Debug`, `header_value`, etc. | near-identical re-implementation | Duplicated, must be kept in lockstep by hand |
| transport error | `HttpError` (transport + status classes) | `TransportError` (transport only) | Intentionally narrower |

This is exactly the duplicate-seam risk the task flags. The redaction guarantee is the load-bearing one, and
it currently lives in **two** hand-maintained `SENSITIVE_HEADERS` arrays and **two** manual redacting `Debug`
impls. If t18 ever adds a sensitive header (e.g. `x-goog-api-key`, plausible for Google) and t19's copy is not
updated, the adapter copies header *values* across the seam and t19's redaction would silently miss it — a
latent leak introduced by drift, not by this commit.

**Recommendation (structural, before three drivers build the adapter three times):** extract the **pure,
runtime-free** request/response/method/redaction layer into a new leaf crate `qfs-http-core` that BOTH
`qfs-driver-http` and `qfs-google-auth` depend on:

- `qfs-http-core` owns: `HttpMethod`, `HttpRequest`, `HttpResponse`, `SENSITIVE_HEADERS`,
  `is_sensitive_header`, and the redacting `Debug` impls. It depends on `qfs-secrets` (for `REDACTED`) →
  types only. **It is a leaf w.r.t. the runtime** — it carries no `reqwest`/`tokio`, so it does not widen
  tokio's reach and the confinement test stays green.
- `qfs-driver-http` keeps `HttpClient` (sync trait), `ReqwestClient`, `HttpError` (the status/transport
  taxonomy), and the runtime bridge — and re-exports the DTOs from `qfs-http-core`.
- `qfs-google-auth` depends on `qfs-http-core` for the DTOs + redaction, and keeps **only** its `HttpExchange`
  trait + `MockExchange` locally (the trait is its seam; the DTOs are shared). The single `SENSITIVE_HEADERS`
  array and single redacting `Debug` then cannot drift — the leak-by-drift risk is eliminated by construction
  rather than by review vigilance.
- `dep_direction.rs` gains one assertion: `qfs-http-core` is a leaf and is depended on by both
  `qfs-driver-http` and `qfs-google-auth`; neither re-defines the DTOs.

Why now rather than later: the task is explicit that *three* Google drivers (t20/t21/t41) each supply an
`HttpExchange`-over-`HttpClient` adapter. With shared DTOs, each adapter is a true zero-copy pass-through
(same `HttpRequest`/`HttpResponse` type on both sides — the adapter only bridges the *trait*, `send` vs
`exchange`, and lowers `HttpError`→`TransportError`). With the current duplicated DTOs, each adapter must
**field-copy** `HttpRequest`→`HttpRequest'` and `HttpResponse'`→`HttpResponse` three times, and every such copy
is a place a future field addition can silently drop a header or a body. Extracting the core crate collapses
three hand-written DTO copies into three trivial trait shims and removes the drift surface before it
multiplies. This is the durable structure to lock in before the drivers build on it.

If the team prefers to defer the crate extraction, the minimum mitigation is a **cross-crate guard test** in
`qfs-google-auth` that asserts its `SENSITIVE_HEADERS` is a superset-or-equal of `qfs_driver_http::SENSITIVE_HEADERS`
(a `dev-dependency` on `qfs-driver-http` for tests only does not affect the production dep graph). That pins
the redaction set against drift without the crate split — but the crate split is the better answer and the one
I recommend.

## 3. OAuth correctness — PASS

- **Auth-code + refresh flow correct.** `exchange_code` posts `grant_type=authorization_code` with
  `code/redirect_uri/client_id` + `client_secret`; `refresh_access_token` posts `grant_type=refresh_token`
  with `refresh_token/client_id` + `client_secret`. Both parse the owned `TokenResponse` and require
  `access_token`; `exchange_code` additionally requires `refresh_token` and fails typed (`Invalid`) if absent —
  the right fail-closed behavior (test `exchange_without_refresh_token_is_invalid`).
- **`access_type=offline` + `prompt=consent`** are both set in `build_auth_url`, the correct pair for a
  reliably-returned refresh token on re-consent. Asserted by `auth_url_carries_localhost_offline_consent_and_scopes`.
- **401 → refresh → retry-once, no infinite loop.** `GoogleApiClient::send` calls `send_once`, returns on
  non-401, else `invalidate()` + a single `send_once` whose result is returned unconditionally — a second 401
  is handed back, never re-looped. Bounded. (test `api_client_refreshes_and_retries_once_on_401`,
  `api_client_passes_through_non_401_without_retry`).
- **`invalid_grant` → reauthorize, no retry storm.** `token_error_from_body` surfaces the OAuth `error` slug as
  `TokenRefresh { reason: "invalid_grant" }`; `is_reauthorize_required()` keys on exactly that. The refresh path
  does **not** retry on `invalid_grant` — it returns the error up. The caller contract (re-authorize, don't
  loop) is documented and testable. (test `invalid_grant_maps_to_typed_token_refresh_error`).
- **Expiry skew sound.** `AccessToken::from_lifetime` does `lifetime.saturating_sub(skew)` then
  `now.saturating_add(usable)`; a lifetime at/below the 60s skew yields an already-expired token (forces
  immediate refresh) rather than underflowing — correct saturating arithmetic.
- **Clock injection makes expiry testable without sleep.** `Clock` trait + `ManualClock` (atomic, `advance`)
  drives `stored_source_caches_then_refreshes_exactly_once_on_expiry` deterministically. `is_expired` is a
  pure monotonic comparison. Good seam.

One observation: `accept_redirect` sets a read timeout on the *accepted* stream but the `listener.accept()`
itself blocks unbounded (the doc comment is honest about this — "the accept itself blocks; for the CLI a human
is present"). For the interactive CLI flow this is acceptable; if `authorize` is ever driven headless/in a
test harness expecting a hard wall-clock bound, the accept should be wrapped (e.g. `set_nonblocking` + a poll
loop against the deadline, or a watchdog). Non-blocking for t19's scope; flag for whoever wires the CLI command.

## 4. Multi-account — PASS

- **Encoding injective/reversible/safe.** `encode_account_email` escapes `%`→`%25` first (so the escape char
  itself round-trips), then `@`→`%40`, `/`→`%2f`, and any whitespace byte-wise. Because `%` is escaped first,
  the encoding is prefix-unambiguous and injective; `decode_account_email` inverts it. The
  `a@b.com` vs `a%40b.com` distinctness test confirms no collision between a literal `%`-string and an encoded
  `@`. Result is always t27-`AccountId`-valid (no `@`/`/`/whitespace — confirmed against `AccountId::new`'s
  reject set in `crates/secrets/src/key.rs`). Empty email is rejected typed. (test
  `email_account_key_encoding_round_trips_and_is_distinct`).
- **Independent per-account caches.** Each `StoredTokenSource` holds its own `Mutex<Option<AccessToken>>` and
  its own `account_email`; two emails → two sources → two refresh-token lookups → two caches (test
  `two_accounts_resolve_to_independent_token_sources`). No shared mutable cache across accounts.
- **Resolves via t27.** `refresh_token_key` builds `CredentialKey::new(DriverId::new("google"), AccountId)` —
  the t27 `(driver, account)` key. Account *selection* (which email) is correctly left upstream to the t27
  resolve ladder; this crate takes the resolved email. Clean boundary.

Minor robustness note: `decode_account_email` uses `String::from_utf8_lossy`, so a hand-corrupted stored key
with an invalid UTF-8 `%XX` sequence would lossily decode rather than error. Since the only producer is
`encode_account_email` (always valid), this is a defensive-decode-of-trusted-input situation and acceptable;
worth a one-line note that decode assumes encode-produced input.

## 5. localhost redirect — PASS

`LOOPBACK_REDIRECT_HOST = "localhost"`; `redirect_uri(port) = "http://localhost:<port>"`. `authorize` binds
`("127.0.0.1", 0)` (the interface) and advertises `http://localhost:<port>` (the URI). The
bind-IP / advertise-hostname split is exactly right and matches the memory note
(`oauth-loopback-redirect`: must use `http://localhost`, not `http://127.0.0.1`, or silent consent hangs).
Pinned by `redirect_uri_host_is_localhost_not_ip` and re-asserted in the auth-URL and token-exchange tests
(the `redirect_uri` in the exchange must match the one in the auth URL — both `localhost`). Honored.

## 6. PKCE park honest? + spine/wasm cleanliness — PASS

- **PKCE park is honest.** lib/oauth docs and the ticket state PKCE is parked in favor of `client_secret` +
  `state` per the Go model. `state` *is* implemented as a real CSRF guard (`new_state` + state-before-code
  validation in `parse_redirect_request`), so the park does not leave the flow defenseless — it drops the
  PKCE code_verifier/challenge, not the CSRF protection. The `state` generator is explicitly documented as
  unpredictable-not-cryptographic, which is the correct honesty for a single-use, loopback-validated token.
  Reasonable for a Desktop client with a `client_secret`; I'd suggest a follow-up ticket to add PKCE
  (`S256`) anyway, since Google now recommends PKCE even for confidential desktop clients and it is cheap to
  add on top of the existing `state` plumbing — but parking it here is defensible and documented.
- **wasm clean.** `authorize` (the only socket/listener code) is `#[cfg(not(target_arch = "wasm32"))]` at both
  the module and the `pub use`. The refresh-only path (`OAuthClient`, `StoredTokenSource`, `GoogleApiClient`,
  `HttpExchange`) carries no `std::net`/listener and is synchronous, so the wasm32 refresh build is clean by
  construction. (I did not *execute* a wasm build — Architect is analytical-only — but the cfg-gating is
  structurally correct and the non-gated modules contain no socket/thread/`Instant`-on-wasm hazards beyond
  `SystemClock`'s `Instant::now`, which is available on wasm32.)

## Concerns + proposals summary (Critical Review Policy)

1. **Duplicate HTTP DTO seam already drifting (the headline structural concern).** *Proposal*: extract
   `qfs-http-core` (leaf) owning `HttpMethod`/`HttpRequest`/`HttpResponse`/`SENSITIVE_HEADERS`/redacting
   `Debug`, depended on by both `qfs-driver-http` and `qfs-google-auth`, so the redaction set and DTOs cannot
   drift and the three driver adapters become zero-copy trait shims. Minimum fallback if deferred: a
   cross-crate test asserting t19's `SENSITIVE_HEADERS ⊇ qfs_driver_http::SENSITIVE_HEADERS`.
2. **`HttpMethod` divergence** (t18 is `#[non_exhaustive]` 4-variant; t19 is closed 2-variant). Folded into
   proposal 1 — sharing the type removes the divergence.
3. **`authorize` accept is unbounded** despite the `timeout` parameter (only the read is bounded). *Proposal*:
   for any non-interactive caller, wrap accept in a deadline-aware poll; fine to defer to the CLI-wiring ticket.
4. **PKCE deferred.** *Proposal*: follow-up ticket to add `S256` PKCE on top of the existing `state` plumbing;
   acceptable to park for t19.

None of these blocks the auth base. Token safety is correct today; the structural fix (1) should land before
t20/t21/t41 multiply the duplicated adapter.
