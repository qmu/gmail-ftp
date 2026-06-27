---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260626100600-t48-oauth-as-metadata-prm-jwks.md, 20260626100300-t45-identity-users-accounts-local-signup.md]
---

# t56 — Upstream OIDC federation (hub model)

## Overview

Delivers **decision D** within milestone **M5**: a self-hosted or local qfs server can trust
qfs Cloud (or any upstream OIDC IdP) so **one identity reaches a laptop, the office server, and
the managed cloud** without a separate login per place (roadmap §3.1, §4.1). This adds an OIDC
**relying-party (RP)** path that sits *alongside* the authorization server t48 already serves —
t48 made qfs its own AS (PRM/RFC 9728, AS-metadata/RFC 8414, JWKS); t56 makes qfs *also* a
client of an upstream AS for the human-login leg. The id-token-verification primitives (RS256/
ES256 over `crypto-core`, JWKS handling) exist as a library after t48; the local `users`/
`accounts` tables and sign-up exist after t45. What is genuinely **new**: an outbound OIDC
discovery + authorization-code login against an upstream issuer, **upstream id-token
verification** (issuer/audience/expiry/signature against the *upstream's* fetched JWKS), and the
mapping of an upstream subject → a local `users`/`accounts` row (the `accounts` table is exactly
the "linked sign-in identities" store decision B reserved).

## Exact seams

- t48's OAuth crate (new `qfs-oauth`) — reuse its JWT verify (RS256/ES256 over
  `crates/crypto-core` `sha256`/`hmac_sha256`/`constant_time_eq` + minimal asn.1) and JWKS
  parsing for verifying **upstream** id-tokens; add the RP-side discovery + code exchange. Do NOT
  pull a heavy OIDC SDK — extend the thin path, or justify a vetted minimal dep.
- `qfs-identity` (t45) over the System DB — `accounts` (linked sign-in identities) is where an
  upstream `(issuer, subject)` is recorded and linked to a `users` row; add
  `link_or_create_from_oidc(claims)` that finds-or-provisions the local user. Reuse t45's
  sign-up, not a parallel path.
- t42 System DB + migrations — new columns/table for federated identity providers (issuer URL,
  client id, cached JWKS) and the `(issuer, subject)` linkage; a new idempotent migration.
- `crates/google-auth/src/lib.rs` — the existing `OAuthClient` (`build_auth_url`,
  `exchange_code`) and the runtime-free `HttpExchange` seam are the **pattern** for the outbound
  auth-code + token-exchange leg against the upstream issuer; the JWKS/discovery fetch rides the
  same synchronous exchange seam (and the one real `crates/qfs/src/transport.rs` `HttpTransport`).
- `crates/http/src/serve.rs` / `route.rs` / `handler.rs` / `params.rs` — the RP callback
  (`redirect_uri`) is served over the in-house listener; the upstream `code`/`state` arrive as
  untrusted params bound through `params.rs`. PKCE state from t49 is reused for the RP leg.
- t46 sessions — a successful federated login establishes a local session exactly as local
  sign-in does, so downstream code never distinguishes "how" the human authenticated.
- `crates/http-core/src/lib.rs` `SENSITIVE_HEADERS`/`is_sensitive_header` — keep upstream tokens
  out of logs through the single redaction authority.

## Implementation steps

1. **Provider config + migration (green).** Add a federated-provider record (issuer, client id,
   client secret as `Secret`, discovery/JWKS cache) and an `accounts` linkage shape via a new t42
   migration. Pure rusqlite; idempotent-apply test.
2. **OIDC discovery + JWKS (pure verify, mock HTTP).** Implement `.well-known/openid-configuration`
   discovery and upstream JWKS fetch/cache, then **id-token verification** (signature against
   fetched JWKS, `iss`/`aud`/`exp`/`nonce` checks) reusing t48's verify primitives. Golden tests
   against a mock issuer; no live IdP.
3. **RP login leg.** Build the outbound auth-code request (reuse `OAuthClient`/`HttpExchange`
   pattern, PKCE from t49), serve the callback over `crates/http` binding `code`/`state` via
   `params.rs`, exchange for tokens, verify the id-token, extract claims.
4. **Subject → local identity mapping.** Implement `link_or_create_from_oidc(claims)` in
   `qfs-identity`: match `(issuer, subject)` in `accounts`, else provision a `users` row + link,
   then establish a t46 session. Test the three cases (existing link, new user, claim mismatch).
5. **Honest docs + version.** Document federated sign-in in `docs/guide/*` only once verified
   end-to-end against a mock; bump patch in `crates/qfs/Cargo.toml`; run
   `cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/oauth/src/*` (t48 `qfs-oauth`) — RP discovery, id-token verification, JWKS fetch/cache.
- `crates/identity/src/*` (t45 `qfs-identity`) — `accounts` linkage, `link_or_create_from_oidc`.
- New System-DB migration (federated providers + `(issuer, subject)` linkage).
- `crates/qfs/src/serve.rs` — RP callback route wiring.
- `crates/http/src/route.rs` / `handler.rs` / `params.rs` — the callback (untrusted code/state).
- `docs/guide/*` — federated sign-in (honest, post-ship).

## Considerations

- **Verify, never trust.** An upstream id-token is untrusted until its signature validates
  against the *upstream's* JWKS and `iss`/`aud`/`exp`/`nonce` all check out. This is the
  load-bearing security detail — a malformed/forged token must fail closed, never provision a
  user. Keep verification pure and unit-testable; the only impure step is the JWKS/token fetch.
- **Identity ≠ authorization (§4.1).** Federation only answers *who you are*; it grants zero
  capability. A freshly-federated user is default-deny until `POLICY`/membership (t55/t57) grants
  access. Do not let "came from a trusted IdP" imply authorization.
- **Secrets discipline.** Upstream client secret, codes, and tokens are `qfs_secrets::Secret`,
  redacted via `http-core` `SENSITIVE_HEADERS`, never logged or in error `Display`.
- **Hub vs. AS roles stay distinct.** t48 = qfs *is* an AS (for Claude/MCP); t56 = qfs *trusts*
  an upstream AS (for human login). Keep the two code paths and route tables clearly separate so
  the AS surface is never confused with the RP callback.
- **Dep-direction & wasm.** RP login is native/server-side; verification primitives stay pure and
  wasm-buildable. New edges land on `crates/qfs`; `qfs-cmd` stays clean.
- **Open decision (flag).** Auto-provisioning policy (does a verified upstream user get a local
  `users` row automatically, or wait for an invite/t55?) and the trust model (which issuers a
  host will accept) are operator choices — flag, don't hardcode.
- **Versioning.** One PR + patch bump in `crates/qfs/Cargo.toml` + `v0.0.x` tag on ship.
