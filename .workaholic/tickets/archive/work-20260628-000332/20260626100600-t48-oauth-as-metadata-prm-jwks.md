---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: L
commit_hash: 4c10ffc
category: Added
depends_on: [20260626100300-t45-identity-users-accounts-local-signup.md]
---

# t48 — OAuth 2.1 AS: PRM (RFC 9728) + AS metadata (RFC 8414) + JWKS

## Overview
Delivers the discovery + key-publication half of making a qfs server **its own OAuth/OIDC
authorization server**: Protected Resource Metadata (RFC 9728) pointing at the AS, Authorization
Server metadata (RFC 8414), a JWKS endpoint, and the signing-key machinery behind it. This is
roadmap **M2** (Server-as-MCP + OAuth AS) and implements **decision C** (a qfs server is its own
authorization server so it can be a remote MCP server) and §4.1 (authorization, kept separate
from identity t45). It is the first two steps of the §2.2 handshake (client discovers PRM → reads
AS metadata); the dynamic-client-registration + auth-code/PKCE flow is **t49**, and guarding the
MCP endpoint with the issued bearer tokens is **t50**. **What already exists:** an OAuth *client*
(`crates/google-auth` — for qfs connecting OUT to Google), the in-house HTTP listener with its
route table (`crates/http`), the pure crypto leaf (`crates/crypto-core`), and the System DB +
identity (t42, t45). **What is genuinely new:** there is NO authorization-server, NO OAuth-AS, NO
JWT/JWKS code anywhere — qfs has only ever been an OAuth *client*. This ticket builds the AS's
static discovery documents + signing keys; it issues NO tokens yet (that is t49/t50).

## Exact seams
- **New crate `qfs-oauth`** (pure-ish core): the AS domain. RFC 8414 AS-metadata and RFC 9728 PRM
  document builders (pure, serde-serializable), a JWKS document builder, and the token-signing
  primitives (RS256 or ES256). Signing/verification is built thinly over `crates/crypto-core`
  plus a *minimal* ASN.1/key encoder — do NOT pull a heavy JWT SDK; if a vetted minimal dep is
  unavoidable (e.g. for RSA/ECDSA bignum), justify it explicitly in the PR and keep it out of the
  pure cores. (Note: `crates/crypto-core` today is `sha256`/`hmac_sha256`/`constant_time_eq` only,
  ZERO deps — keep it that way; asymmetric signing lives in `qfs-oauth`, not `crypto-core`.)
- `crates/crypto-core/src/lib.rs` — reuse `sha256`/`sha256_hex`/`hex_lower` for the JWS signing
  input digest and key thumbprints (`kid`). Constant-time compares where relevant. Pure leaf.
- `crates/http/src/route.rs` `Router`/`RoutePattern`/`compile_endpoint` and `src/handler.rs`
  `dispatch` (or the `Fallback` seam in `serve.rs`) — serve three well-known GET routes over the
  existing `tokio::net::TcpListener`: `/.well-known/oauth-protected-resource` (PRM),
  `/.well-known/oauth-authorization-server` (AS metadata), and `/jwks.json` (JWKS). Read-only,
  cacheable, no credentials — they pass `crates/http/src/policy.rs` `assert_read_only` trivially.
- `crates/http-core/src/lib.rs` — `HttpResponse` DTO + `SENSITIVE_HEADERS`. These are *public*
  documents (no auth) but the PRIVATE signing key must NEVER appear in any response or log; only
  public JWK material is published at `/jwks.json`.
- `crates/store` (System DB, **t42**) — add an `0004_oauth_keys` migration storing the AS signing
  keypair(s): `oauth_keys (kid PK, alg, public_jwk, private_key_encrypted, created_at, status)`.
  The private key is **envelope-encrypted at rest** reusing the t43 data-key wrap (decision E) —
  same mechanism that protects connection secrets. `schema_version` bump via t42's runner.
- `crates/secrets/src/secret.rs` `Secret` — wrap the unwrapped private key in memory (redacted,
  zeroized after signing). Never `String`/`Vec<u8>` bare.
- `crates/qfs/src/serve.rs` (`run_serve` → `qfs_http::serve_config_full`) — composition root;
  load/generate the signing key on boot (decrypt via the t43 data-key), register the three
  well-known routes, inject the key handle into whatever t49/t50 will use to mint/verify tokens.
- `crates/cmd/tests/dep_direction.rs` — add `qfs-oauth` to allowlists; pure core, tokio only at
  the binding. Live key I/O lands on `crates/qfs`.

## Implementation steps
1. **Signing-key model + migration (tree green).** Add `0004_oauth_keys` to t42's runner. On
   first boot in the binary, generate one signing keypair (RS256 or ES256 — pick one, see open
   decision), wrap the private key with the t43 data-key, store it, publish the public JWK. Assert
   idempotent migration + that a second boot reuses the existing active key.
2. **`qfs-oauth` document builders (pure).** Implement the RFC 8414 AS-metadata builder (issuer,
   authorization_endpoint, token_endpoint, registration_endpoint, jwks_uri, response_types,
   code_challenge_methods=["S256"], grant_types — pointing at the endpoints t49 will serve), the
   RFC 9728 PRM builder (resource = the MCP endpoint, authorization_servers = [issuer]), and the
   JWKS builder (public key → JWK with `kid`/`alg`/`use=sig`). Golden-test each document shape.
3. **Signing primitives (pure-ish).** Implement `sign_jws(claims, &SigningKey) -> compact_jws`
   and `verify_jws(token, &Jwks) -> Result<Claims>` thinly over the chosen alg + `crypto-core`
   digest. No token issuance policy here — just the primitive, unit-tested against a fixed-key
   golden vector. (t49/t50 consume these to mint/verify access/refresh tokens.)
4. **Serve the three routes.** Wire `/.well-known/oauth-protected-resource`,
   `/.well-known/oauth-authorization-server`, and `/jwks.json` into the `crates/http` route table
   from `crates/qfs/src/serve.rs`; serve cached, public, read-only JSON. The issuer/base-URL is
   derived from the listener's advertised address (mirror how `crates/google-auth` `authorize()`
   advertises `http://localhost:<port>` for loopback).
5. **Boot wiring + key rotation seam.** Load + decrypt the active signing key on boot into a
   `Secret`; expose a key handle for t49/t50. Support multiple `oauth_keys` rows (one active, old
   ones `status=retiring`) so JWKS can publish overlapping keys during rotation — implement the
   schema + JWKS multi-key publication now, leave the rotation *trigger* as a documented seam.
6. **Docs + version.** Add `qfs-oauth` to `dep_direction.rs`. Update §2.2 status truthfully (PRM/
   AS-metadata/JWKS are served; NO tokens issued yet — say exactly that). `cargo run -p xtask --
   gen-docs --check`; patch-bump `crates/qfs/Cargo.toml`.

## Key files
- `crates/oauth/` (new): `Cargo.toml`, `src/lib.rs`, `src/metadata.rs` (RFC 8414 + RFC 9728
  builders), `src/jwks.rs`, `src/sign.rs` (JWS sign/verify), `src/key.rs` (`SigningKey`, JWK).
- `crates/store/src/migrations/0004_oauth_keys.sql` + `schema_version` bump (t42 runner form).
- `crates/store/src/oauth_key_store.rs` (new): rusqlite store for the envelope-encrypted keypair.
- `crates/qfs/src/serve.rs` (modify): generate/load key, register the three well-known routes.
- `crates/cmd/tests/dep_direction.rs` (modify): allowlist `qfs-oauth`.
- `crates/qfs/Cargo.toml` (modify): patch bump.

## Considerations
- **Safety floor.** The three discovery routes are pure reads (`assert_read_only` passes) and
  publish only public metadata + public JWK material. The private signing key is the crown jewel:
  envelope-encrypted at rest (t43 data-key), `Secret`-wrapped + zeroized in memory, NEVER in a
  response body, log, trace, or audit entry. No bare `Vec<u8>`/`String` private-key handling.
- **Don't bloat the dep tree (decision C, honestly).** Building a thin RS256/ES256 over
  `crypto-core` keeps the closed, auditable surface. If a minimal vetted bignum/ECDSA dep is
  required, justify it in the PR, pin it, and keep it inside `qfs-oauth` — never in `crypto-core`
  (which must stay zero-dep) or the wasm-bound pure cores.
- **Identity ≠ authorization (§4.1).** This crate is purely the authorization-server machinery; it
  does not touch `qfs-identity` (t45) directly here — the human-login link is wired in t49 where
  the auth-code flow consults the identity store. Keep the two crates decoupled.
- **Honesty.** Serving discovery documents that ADVERTISE endpoints (token/registration) which do
  not exist yet would mislead a client. Either (a) gate the advertised endpoints behind t49 so
  metadata only lists what is live, or (b) ship metadata + endpoints together. Prefer (a): in t48,
  metadata lists jwks_uri + issuer; advertise token/authorization/registration endpoints only when
  t49 serves them. Flag this sequencing in the PR.
- **Dep-direction.** `qfs-oauth` is a pure-ish leaf; tokio only at the `crates/http` binding; key
  I/O (rusqlite, sync) is the binary-injected `crates/store` layer. Add to `dep_direction.rs`.
- **Open product decisions to flag (do not guess).** (a) RS256 vs. ES256 (ES256 = smaller keys,
  simpler ASN.1; pick and justify). (b) Issuer URL derivation when behind the trusted reverse
  proxy (decision F) — the proxy may terminate TLS and rewrite host; the issuer must match what
  the client sees. Note this as a constraint t49/t50 must honor. (c) Key-rotation cadence /
  trigger (schema supports it; policy deferred).
- **Versioning.** One PR, one patch bump, a `v0.0.x` tag on ship.
