---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Config]
effort:
commit_hash: 8120c39
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t27-credential-secret-store-and-resolution.md]
---

# Google OAuth + multi-account auth (shared base)

## Overview

This delivers the shared Google OAuth2 substrate that both the Gmail and Drive
drivers depend on (RFD §5 Driver contract, §10 Security, epic E5 Auth/credentials
feeding E4 Drivers). A single Google Cloud "Desktop" OAuth client is used with a
**loopback redirect** flow to obtain user consent, exchange the code for tokens,
refresh access tokens on expiry, and persist per-account refresh tokens through the
encrypted credential store (ticket t27). Account identity is resolved from the
authenticated profile email so multiple Google accounts can be mounted side by side
(e.g. `/gmail/<email>/...`, `/drive/<email>/...`).

Per RFD §10, tokens are secrets: never logged, stored encrypted, and resolved
lazily. Per RFD §171, no heavy vendor SDK — a thin HTTP client only. This ticket is
pure infrastructure: it produces an authenticated token source; it constructs no
effect-plans and performs no driver I/O itself.

## Scope

In scope:
- Google OAuth2 Desktop-app flow with **`http://localhost` loopback** redirect.
- Authorization-code exchange, access-token refresh, expiry tracking.
- Per-account refresh-token storage/retrieval via the credential store.
- Account identity via Google `getProfile`-style userinfo email lookup.
- A reusable `TokenSource` consumed by Gmail/Drive drivers.
- Scope-set parameterization (caller passes required scopes; this crate is scope-agnostic).

Out of scope:
- Gmail driver namespace/schema/procedures — deferred to the Gmail driver ticket.
- Drive driver namespace/schema — deferred to the Drive driver ticket.
- Credential store implementation (encryption, on-disk format) — owned by **t27**.
- Driver contract trait definitions — owned by **t13**.
- Server-side `POLICY`/capability enforcement — deferred to the server epic (E7).

## Key components

New crate/module `crates/cfs-google-auth` (or `src/auth/google/`):

- `struct OAuthClient { client_id, client_secret, scopes: Vec<String>, http: HttpClient }`
  — thin reqwest-based client; no vendor SDK (owned DTOs only, RFD §171).
- `struct GoogleAccount { email: String, refresh_token: SecretString }` — owned DTO;
  `email` is the account key, never a vendor `Userinfo` type leaking out.
- `struct AccessToken { value: SecretString, expires_at: Instant }`.
- `trait TokenSource { async fn access_token(&self) -> Result<AccessToken, AuthError>; }`
  — what Gmail/Drive drivers depend on; refreshes transparently behind the trait.
- `struct StoredTokenSource { account_key, store: Arc<dyn CredentialStore>, oauth: OAuthClient }`
  — implements `TokenSource`; loads refresh token from t27's store, caches access token.
- `async fn authorize(&self) -> Result<GoogleAccount, AuthError>` — runs loopback flow:
  binds an ephemeral `TcpListener` on `127.0.0.1:0`, advertises redirect URI as
  **`http://localhost:<port>`** (the host string must be `localhost`, not the IP, to
  avoid the silent-consent stall on Desktop clients), opens consent URL, captures `code`.
- `async fn fetch_profile_email(&AccessToken) -> Result<String, AuthError>` — calls
  `https://www.googleapis.com/oauth2/v3/userinfo`, returns owned `email`.
- `enum AuthError { Denied, Timeout, Network, TokenRefresh, ProfileLookup, Store(...) }`
  — structured, AI-legible errors (RFD §103).
- Credential-store keys namespaced `google:<email>:refresh_token` (resolution via t27).

## Implementation steps

1. Add the `cfs-google-auth` module; add `reqwest` (rustls), `url`, `serde`,
   `secrecy`, `tiny_http`/`tokio` listener deps (thin, no Google SDK).
2. Define owned DTOs (`GoogleAccount`, `AccessToken`) and `AuthError`.
3. Implement the OAuth endpoints client: build auth URL (`access_type=offline`,
   `prompt=consent` to guarantee a refresh token), token exchange, token refresh.
4. Implement the loopback listener: bind `127.0.0.1:0`, derive port, set redirect URI
   host to `localhost`, generate + verify `state`, serve one request, extract `code`,
   render a "you may close this tab" success page.
5. Implement `fetch_profile_email`; use it to key the account on `authorize`.
6. Persist `refresh_token` under `google:<email>:refresh_token` via the t27 store;
   never write `client_secret`/tokens to logs.
7. Implement `StoredTokenSource`: load refresh token, mint access token, cache until
   `expires_at - skew`, refresh on miss; map 401/`invalid_grant` to `AuthError`.
8. Implement `TokenSource` trait and wire a constructor the driver tickets can call
   with their own scope set.
9. Tests: golden token-exchange/refresh against a mock HTTP server; no live creds.

## Considerations

- **Loopback host gotcha (hard part):** Desktop OAuth clients stall on silent consent
  when the redirect host is `127.0.0.1`; advertise `http://localhost:<port>` while
  binding the loopback interface. Document this prominently — it is the load-bearing detail.
- **Refresh token availability:** Google only returns a refresh token on first consent
  unless `prompt=consent` + `access_type=offline`; set both so re-auth is reliable.
- **Least privilege & secrets (RFD §10):** scopes are caller-supplied (request the
  minimum each driver needs); `client_secret`, codes, and tokens are `SecretString`,
  never logged, never in error `Display`. Storage delegated to the encrypted store (t27).
- **Idempotency/recovery:** token refresh is naturally idempotent; on `invalid_grant`
  (revoked/expired refresh token) surface a typed error instructing re-`authorize`,
  rather than looping. Access-token cache is in-memory, safe to drop and rebuild.
- **Observability:** structured logs at debug for auth-flow lifecycle (port bound,
  code received, refresh performed) with **token values redacted**; emit account email
  only (a low-sensitivity identifier).
- **Purity (RFD §3):** this module performs network I/O for auth, but exposes only a
  `TokenSource`; it constructs no `Plan` and no driver effects, keeping the driver
  layer's effect-as-data invariant intact.
- **wasm32 note:** the loopback `authorize` path is native-only (CLI); on Workers,
  refresh tokens are provisioned out-of-band and only `StoredTokenSource` is used —
  keep `authorize` feature-gated so the refresh path compiles to `wasm32`.
- **Directory/standards:** owned DTOs only at the boundary; no vendor types in public
  signatures; module under `crates/`/`src/auth/` per coding standards.

## Acceptance criteria

- `cargo build` (native and `wasm32` for the refresh-only path) and `cargo clippy
  -- -D warnings` are green.
- Loopback flow advertises redirect URI with host `localhost` (asserted in a unit test
  on the generated redirect URI / auth URL).
- `authorize` returns a `GoogleAccount` whose `email` matches the mocked userinfo
  response; refresh token is persisted under `google:<email>:refresh_token`.
- `StoredTokenSource::access_token` returns a cached token before expiry and triggers
  exactly one refresh after expiry (golden test against mock HTTP server, **no live creds**).
- `invalid_grant` from the refresh endpoint maps to a typed `AuthError::TokenRefresh`,
  not a panic or generic error.
- No secret (client secret, code, access/refresh token) appears in any log line or in
  `AuthError`'s `Display`/`Debug` output (assert via a redaction test).
- Two distinct account emails can be authorized and stored independently and resolved
  back to two distinct `TokenSource`s.
