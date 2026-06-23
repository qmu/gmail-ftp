# Coding Phase E2E Review — Planner — t19 (Google OAuth + multi-account auth base)

- Author: Planner (Progressive)
- Phase / step: coding / review-and-testing
- Role: E2E / external-interface testing only (no code review, no production code)
- Ticket: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t19-driver-google-oauth-multi-account.md`
- Method: A throwaway external-consumer crate (`/tmp/t19_e2e`, own `[workspace]`, path-deps on
  `crates/google-auth` + `crates/secrets`, no production code, removed after) driving ONLY the
  public `cfs_google_auth` API + the provided `MockExchange` (scripted token/API responses,
  recorded requests) + `cfs_secrets::InMemoryStore`. No live Google, no network, no socket.

## Verdict per item

| # | Item | Result |
|---|------|--------|
| 1 | Token exchange (auth-code -> token), form fields, tokens stored as Secret | **PASS** |
| 2 | Auth URL / redirect: Google endpoint, offline+consent, scopes, localhost (NOT 127.0.0.1) | **PASS** |
| 3 | Refresh on expiry (ManualClock): cached before, refresh POST after | **PASS** |
| 4 | 401 -> refresh -> retry exactly once; persistent 401 does not loop | **PASS** |
| 5 | Multi-account: independent sources/caches, injective+reversible encoding, right token used | **PASS** |
| 6 | Token safety: canary nowhere on Debug/Display/log; invalid_grant -> structured error, no panic | **PASS** |

## Overall verdict: **E2E approved**

No token leak. No `127.0.0.1` redirect (the memory-critical loopback-host gotcha is honored). No
panic on `invalid_grant`. All six scenarios pass from the outside through the public API only.

---

## Evidence

### Item 1 — Token exchange
`OAuthClient::exchange_code(code, redirect_uri, now)` produced the minted `AccessToken` (bearer ==
canary access token) and a refresh `Secret` (exposes the canary refresh token; its `Debug` shows
`***redacted***`). The refresh `Secret` was stored via `InMemoryStore::put` under
`refresh_token_key("alice@example.com")` and retrieved back as a `Secret`.

Recorded POST to the token endpoint (exactly one request; method POST; URL == `TOKEN_ENDPOINT`
`https://oauth2.googleapis.com/token`):

```
grant_type=authorization_code&code=CANARY-AUTH-CODE-zz00yy11&redirect_uri=http%3A%2F%2Flocalhost%3A54321&client_id=client-id-123.apps.googleusercontent.com&client_secret=CANARY-CLIENT-SECRET-99887766
```

Asserted form fields: `grant_type=authorization_code`, `code` present and correct,
`redirect_uri` == the `localhost:<port>` redirect, `client_id` present. (The `client_secret` rides
the same wire body — that is the on-the-wire request Google receives, read directly from
`req.body`; see Item 6 for the proof that NO log/Debug/error surface ever renders it.)

### Item 2 — Auth URL / redirect (memory-critical)
`OAuthClient::redirect_uri(49876)` == `http://localhost:49876` — host is `localhost`.

```
redirect_uri: http://localhost:49876
auth URL:
https://accounts.google.com/o/oauth2/v2/auth?client_id=client-id-123.apps.googleusercontent.com&redirect_uri=http%3A%2F%2Flocalhost%3A49876&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.readonly+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.readonly&access_type=offline&prompt=consent&state=state-csrf-token
```

Asserted: starts with `AUTH_ENDPOINT`; contains `access_type=offline`, `prompt=consent`,
`response_type=code`, the requested scopes, and `state`. **`redirect_uri` contains `localhost`,
does NOT contain `127.0.0.1`, and the full auth URL contains `127.0.0.1` nowhere.** This is the
load-bearing Desktop-client silent-consent gotcha; it is correctly avoided.

### Item 3 — Refresh on expiry (ManualClock)
- First `access_token()` (cache miss) -> `access-A`, fired 1 token POST.
- After `advance(100s)` (< 3600s - 60s skew): still `access-A`, **no additional POST** (cached).
- After `advance(+4000s)` (past expiry): -> `access-B`, **2nd POST fired**.
- The 2nd POST body: `grant_type=refresh_token&refresh_token=refresh-bob&client_id=...&client_secret=...`
  (i.e. a refresh grant, using the stored refresh token).

### Item 4 — 401 -> refresh -> retry exactly once
Scripted FIFO: initial-refresh(200) -> API(401) -> re-refresh(200) -> API(200). `GoogleApiClient::send`
returned the final 200 (body `{"ok":true}`) after **exactly 4 exchanges** — one refresh + retry.
Persistent-401 variant (initial-refresh -> 401 -> re-refresh -> 401): `send` returned the 2nd 401
with **exactly 4 exchanges** — it retried once and then surfaced the 401 rather than looping.

### Item 5 — Multi-account
`alice@example.com` -> account id `alice%40example.com`; `bob+tag@example.com` ->
`bob+tag%40example.com`. Encoded ids are distinct (injective), `decode_account_email` round-trips
both back to the original email (reversible), and neither encoded id contains `@`. Two
`StoredTokenSource`s over one shared store, each scripted its own mock: account A resolved
`access-alice` using `refresh_token=rt-alice`; account B resolved `access-bob` using
`refresh_token=rt-bob` — independent caches, the right account's token used in each.
(Note: `encode_account_email` is private; injectivity/reversibility validated through the public
`refresh_token_key` keying + `decode_account_email`, which is the consumer-visible surface.)

### Item 6 — Token safety (the headline invariant)
Planted four canaries (access token, refresh token, client secret, auth code) through a full
exchange + a stored-token-source mint + an `invalid_grant` refresh. Captured 8 text surfaces:
`AccessToken` Debug, refresh-`Secret` Debug, every recorded `HttpRequest` Debug (whose bodies hold
the secret/code on the wire), `BorrowedToken` Debug, and `AuthError` Display+Debug (twice).
**No canary substring appeared on any of the 8 surfaces**, while the `***redacted***` marker did
appear (proving Debug ran over the secrets). Sample:

```
AccessToken { value: Secret(***redacted***), expires_at_nanos: 3540000000000 }
```

`invalid_grant` from the refresh endpoint mapped to `AuthError::TokenRefresh { reason:
"invalid_grant" }` with `is_reauthorize_required() == true` and `code() == "auth_token_refresh"`,
via both `StoredTokenSource::access_token` and a direct `OAuthClient::refresh_access_token` — a
structured reauthorize-required error, **no panic, no token returned**.

## Concern + proposal (Critical Review Policy)

- Concern (low severity, not blocking): the redirect-URI host correctness lives in the
  `redirect_uri(port)` helper and the `LOOPBACK_REDIRECT_HOST = "localhost"` constant, but the
  native `authorize` loopback path (binding `127.0.0.1:0` and advertising `localhost`) is
  feature-gated and outside this mocked E2E (it needs a real socket), so the wiring of "bind IP /
  advertise localhost" is asserted only at the helper level here.
- Proposal: keep the existing unit assertions on the generated redirect/auth URL (already green),
  and add one native-only integration test that binds an ephemeral `127.0.0.1:0` listener, derives
  the port, and asserts the advertised `redirect_uri` is `http://localhost:<that-port>` — closing
  the loop on the bind-IP-vs-advertise-host split without a live Google round-trip. This is an
  enhancement, not a gate; the observable behavior is correct today.

## Notes
- Throwaway crate built and ran clean (`cargo run`, all assertions PASS); removed after the run.
- No production code was modified by the Planner. No code review performed (out of role).
