---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: L
commit_hash: 44c8a36
category: Added
depends_on: [20260626100600-t48-oauth-as-metadata-prm-jwks.md, 20260626100400-t46-session-handling.md]
---

# t49 — Dynamic client registration (RFC 7591) + authorization-code + PKCE (OAuth 2.1)

## Overview
Delivers the live connection handshake of roadmap §2.2 steps 2–3: dynamic client registration
(RFC 7591, so no manual client setup) and the OAuth 2.1 authorization-code flow with PKCE, where
the human signs in to the qfs identity (t45) and consents, and the session (t46) carries that
login through the flow. This is roadmap **M2** (Server-as-MCP + OAuth AS) and implements
**decision C** with the §2.2 / §4.1 separation: identity authenticates the human (t45), this
authorizes the client to obtain tokens. **What already exists:** the AS discovery + signing
primitives (t48 `qfs-oauth` `sign_jws`/JWKS), the identity store + sign-up (t45), sessions (t46),
the HTTP listener + route table (`crates/http`), and the System DB (t42). **What is genuinely
new:** there is NO authorization endpoint, NO token endpoint, NO client/code storage, NO PKCE
verification anywhere — the OAuth code in the tree is the *client* (`crates/google-auth`), which
performs the inverse of what this ticket builds. This ticket issues the authorization code and
exchanges it for tokens; **guarding the MCP endpoint with those tokens is t50** (this ticket
mints; t50 enforces).

## Exact seams
- `crates/oauth` (the **t48** crate) — extend with the protocol logic this flow needs: the
  authorization-request validation, the PKCE `S256` challenge/verifier check (reuse
  `crates/crypto-core` `sha256` + base64url + `constant_time_eq`), authorization-code minting,
  and the token-endpoint response (access token signed via t48 `sign_jws`, refresh-token handle).
  The protocol decisions stay pure/unit-testable; storage + HTTP are injected.
- `crates/store` (System DB, **t42**) — add an `0005_oauth_clients_codes` migration:
  `oauth_clients (client_id PK, redirect_uris JSON, created_at, ...)` for DCR results,
  `oauth_codes (code_hash PK, client_id, user_id, pkce_challenge, scope, redirect_uri, expires_at)`
  for short-lived auth codes, and a `oauth_refresh_tokens (handle_hash PK, user_id, client_id,
  scope, expires_at, rotated_from)` skeleton (issued here, *enforced/refreshed* in t50). Store
  codes/handles as **hashes**, never plaintext. `schema_version` bump via t42's runner.
- `crates/http/src/route.rs` `Router`/`RoutePattern`/`compile_endpoint`, `src/handler.rs`
  `dispatch`, `src/params.rs` (the typed param-binding seam — the untrusted-input boundary) —
  serve three endpoints on the existing listener: `POST /register` (DCR), `GET /authorize`
  (auth-code, renders a consent screen), `POST /token` (code→token + refresh exchange). Params
  bind as typed values via `params.rs` — never string-spliced.
- `crates/session` (the **t46** crate) — `/authorize` requires an authenticated session
  (`SessionStore::lookup`); if absent, redirect to local sign-in (t45) then back. On consent,
  **rotate** the session (t46 `rotate`) to limit fixation. The session→`user_id` is what the
  minted code/token is bound to.
- `crates/identity` (the **t45** crate) — `/authorize` consults `IdentityStore` to render *who*
  is consenting; sign-in (if needed) goes through t45's local password verify.
- `crates/http-core/src/lib.rs` — `HttpResponse` + `SENSITIVE_HEADERS`. Auth codes, tokens, and
  `Authorization` headers are sensitive; ensure redaction. The consent screen is a minimal
  self-contained HTML response (no external assets — same constraint as the future SPA).
- `crates/secrets/src/secret.rs` `Secret` — wrap codes / tokens / refresh handles in transit;
  zeroize.
- `crates/qfs/src/serve.rs` — composition root; wire the three endpoints + inject the
  store/identity/session/signing-key handles.
- `crates/cmd/tests/dep_direction.rs` — `qfs-oauth` already allowlisted (t48); confirm new
  cross-crate edges (oauth→identity/session via injected traits, not direct deps) respect
  dep-direction. Tokio only at the binding.

## Implementation steps
1. **Storage migration (tree green).** Add `0005_oauth_clients_codes` to t42's runner:
   `oauth_clients`, `oauth_codes`, `oauth_refresh_tokens` (hashes only, short TTLs on codes).
   Idempotent apply; `schema_version` bump.
2. **DCR endpoint (`POST /register`).** Validate the RFC 7591 request (redirect_uris required,
   well-formed), mint a `client_id`, persist it, return the registration response. No client
   secret for public PKCE clients. Reject malformed/over-broad redirect URIs (must be an exact
   allowlist match later). Golden-test the request/response shapes.
3. **PKCE + code minting (pure in `qfs-oauth`).** Implement `S256` verification:
   `sha256(verifier)` base64url-compared constant-time to the stored challenge. Implement
   short-lived, single-use authorization-code minting bound to `(client_id, user_id, scope,
   redirect_uri, pkce_challenge)`. Unit-test code lifecycle + a wrong-verifier rejection.
4. **`GET /authorize` (consent).** Validate `client_id`/`redirect_uri` (exact allowlist), require
   a session (t46); if none, send the human through local sign-in (t45). Render a minimal,
   self-contained consent screen listing the requested scope. On approval, rotate the session,
   mint the code, redirect to `redirect_uri?code=...&state=...`. On denial, redirect with
   `error=access_denied`. No code is issued without an authenticated, consenting user.
5. **`POST /token` (exchange).** Verify the auth code (single-use, unexpired, client/redirect
   match), verify the PKCE verifier, then issue a signed access token (t48 `sign_jws`, bound to
   `user_id`/`scope`) + a refresh-token handle (stored hashed). Burn the code. Structured OAuth
   error responses for every failure mode (`invalid_grant`, `invalid_client`, etc.). Golden-test
   the happy path + each rejection.
6. **Wire + docs + version.** Wire the three endpoints from `crates/qfs/src/serve.rs`; once live,
   update the t48 AS-metadata to advertise `authorization_endpoint`/`token_endpoint`/
   `registration_endpoint` (now that they exist — honesty). Update §2.2 status: the handshake
   through token issuance works; MCP is NOT yet guarded by the token (t50). `cargo run -p xtask --
   gen-docs --check`; patch-bump `crates/qfs/Cargo.toml`.

## Key files
- `crates/oauth/src/flow.rs` (new): authorize-request validation, code minting, token-endpoint
  logic. `crates/oauth/src/pkce.rs` (new): `S256` challenge/verifier verification.
- `crates/oauth/src/client_reg.rs` (new): RFC 7591 DCR validation/response.
- `crates/store/src/migrations/0005_oauth_clients_codes.sql` + `schema_version` bump.
- `crates/store/src/oauth_store.rs` (new): rusqlite store for clients/codes/refresh handles.
- `crates/qfs/src/serve.rs` (modify): `POST /register`, `GET /authorize`, `POST /token`; consent
  HTML; inject identity/session/signing-key handles.
- `crates/cmd/tests/dep_direction.rs` (verify): cross-crate edges via injected traits.
- `crates/qfs/Cargo.toml` (modify): patch bump.

## Considerations
- **Safety-first (this is the auth boundary).** PKCE `S256` is **mandatory** (no plain, no
  code-without-challenge). Authorization codes are short-lived, single-use, and bound to client +
  redirect + user + challenge. Redirect URIs are matched against an **exact allowlist** from DCR —
  no wildcard/substring matching (open-redirect protection). `state` is required and echoed.
  Codes/tokens/refresh handles are stored only as hashes; plaintext lives only in the `Secret`-
  wrapped transit value and the one redirect/response that delivers it.
- **Identity vs. authorization (§4.1) — composed, not conflated.** `/authorize` *authenticates*
  the human via t45 identity + t46 session, then *authorizes* the client by minting a code. The
  token carries `user_id` + `scope`; mapping token→user→policy enforcement is t50. Keep the
  consent decision an explicit human action (no silent auto-consent).
- **Session fixation / CSRF.** Rotate the session (t46 `rotate`) at the consent step. The consent
  POST needs CSRF protection (the double-submit/synchronizer token noted as a t46 seam) since it
  is a state-changing browser action. Flag if t46 left this unbuilt — it must exist before this
  ships.
- **Honesty in metadata.** Do not advertise the token/authorize/registration endpoints in t48's
  AS-metadata until they are live; flip them on in this ticket (the t48 sequencing note).
- **Dep-direction.** `qfs-oauth` stays pure-ish (protocol logic only); it reaches identity/session
  through injected consumer-side traits, not direct crate deps. SQLite store I/O + HTML rendering
  live in the binary-injected layer (`crates/store`, `crates/qfs`). Tokio only at the binding.
- **Open product decisions to flag (do not guess).** (a) Scope vocabulary — what scopes does the
  MCP resource define (e.g. `mcp:read`, `mcp:commit`)? Define a minimal set and note that policy
  (decision I, t57) is the real authorization, scopes are coarse. (b) Whether DCR is open or
  gated (open-DCR is the MCP norm but invites client spam — consider a soft cap/expiry). (c)
  Access-token lifetime vs. refresh-token lifetime (t50 enforces refresh; pick conservative TTLs).
- **Versioning.** One PR, one patch bump, a `v0.0.x` tag on ship.
