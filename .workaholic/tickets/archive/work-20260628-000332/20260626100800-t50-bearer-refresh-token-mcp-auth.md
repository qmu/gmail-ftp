---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: L
commit_hash: a5c91ef
category: Added
depends_on: [20260626100700-t49-oauth-dcr-authcode-pkce.md, 20260626100500-t47-mcp-server-binding-tools.md]
---

# t50 — Bearer + refresh tokens guarding the MCP endpoint

## Overview
Closes roadmap **M2**: MCP tool calls now require a bearer access token, a refresh token keeps the
session alive, and each validated token maps to a `user` and the policy that bounds what it may
do. This is §2.2 step 4 ("the client calls MCP tools with a bearer token; a refresh token keeps
the session alive") and implements **decision C** end-to-end — with t50 in place, Claude can
connect to qfs over MCP+OAuth and drive every service qfs fronts. **What already exists after
t47–t49:** the MCP tool surface + `McpBinding` (t47, currently *unauthenticated*, localhost-only),
the OAuth AS that mints signed access tokens + refresh handles (t48 `sign_jws`/JWKS, t49 token
endpoint), the identity store (t45), and the System DB (t42). **What is genuinely new:** there is
NO token *validation* on any request path today — t49 issues tokens, but nothing consumes them
yet, and the MCP endpoint trusts every caller. This ticket adds the bearer-validation middleware
in front of `McpBinding`, the refresh-token rotation grant, and the token→user→policy mapping.
After this, the MCP endpoint can safely leave localhost.

## Exact seams
- `crates/oauth` (t48/t49 crate) — extend with `verify_access_token(token, &Jwks) -> Result<Claims>`
  (reuse t48 `verify_jws` + JWKS), audience/issuer/expiry checks, and the **refresh grant**:
  `POST /token` `grant_type=refresh_token` validates the stored refresh handle (hashed lookup,
  constant-time), rotates it (issue new refresh, retire old via `rotated_from`), and mints a fresh
  access token. Pure protocol logic; storage/HTTP injected.
- `crates/http/src/handler.rs` `dispatch` and `crates/http/src/serve.rs` — add an
  **authentication middleware** seam in front of the t47 `McpBinding` route (`POST /mcp`): read
  the `Authorization: Bearer <token>` header, verify it, attach the resolved `UserId` + scope to
  the request context; reject missing/invalid/expired tokens with a `401` carrying a
  `WWW-Authenticate: Bearer ...` challenge that points at the AS (RFC 9728 PRM, t48) so a client
  knows where to authorize. This is the seam t47 deliberately left open.
- `crates/http-core/src/lib.rs` — `Authorization` is in `SENSITIVE_HEADERS` (confirm/extend) so
  bearer tokens never appear in logs/traces/audit. `HttpResponse` builds the `401` +
  `WWW-Authenticate`.
- `crates/mcp` (t47 crate) — the `McpEngine` `commit`/`preview`/`describe`/`connections` calls now
  receive the authenticated `UserId` + scope from the middleware. The `commit` tool's policy gate
  (`crates/server/src/policy/enforce.rs` `evaluate`, `gate.rs` `gate_plan`) is keyed by that
  user/scope — token→user→policy. `describe` stays pure (still no creds).
- `crates/server/src/policy/` — `model.rs`/`enforce.rs`/`gate.rs`: the existing default-deny,
  pure policy evaluator. This ticket *feeds it a principal* (the token's user/scope); it does not
  change the evaluator. (The richer role/group ACL language is decision I / t57.)
- `crates/core/src/security.rs` `IrreversibleGuard`/`RunMode`/`NeedsPreview` — an authenticated
  MCP `commit` of an irreversible plan still requires the ack path; the selectable safety mode
  (t59) decides auto-vs-approve. Until t59, default to require-approval for irreversible.
- `crates/store` (System DB, **t42**) — uses the `oauth_refresh_tokens` table from t49 for refresh
  rotation; no new table strictly required (add a `last_used_at` column if useful for revocation
  hygiene — small migration `0006_token_use`, optional). `schema_version` bump only if a column is
  added.
- `crates/secrets/src/secret.rs` `Secret` — wrap the incoming bearer + the new tokens; zeroize.
- `crates/qfs/src/serve.rs` — composition root; inject the JWKS/verification handle + refresh
  store into the MCP middleware; once auth is enforced, allow a non-localhost bind (behind the
  trusted reverse proxy, decision F).
- `crates/cmd/tests/dep_direction.rs` — no new crate; verify the middleware edges (http →
  oauth-verify via injected trait) respect dep-direction. Tokio confined to the binding.

## Implementation steps
1. **Access-token verification (pure).** In `qfs-oauth`, implement `verify_access_token` over t48
   `verify_jws` + JWKS, checking signature, `iss` (the t48 issuer, proxy-aware), `aud` (the MCP
   resource), and `exp`. Unit-test against fixed-key golden tokens incl. expired/wrong-aud/
   tampered-signature rejections.
2. **Refresh grant + rotation.** Extend `POST /token` with `grant_type=refresh_token`: hashed
   lookup of the refresh handle, constant-time compare, single-use rotation (`rotated_from`),
   mint a new access token + new refresh handle. Reject reused/revoked/expired handles
   (`invalid_grant`). Golden-test rotation + replay rejection.
3. **Auth middleware in front of MCP.** In `crates/http`, add the bearer-extraction +
   verification step for the `POST /mcp` route. On success attach `UserId`+scope to context; on
   failure return `401` + `WWW-Authenticate: Bearer resource_metadata="..."` pointing at the t48
   PRM. Confirm `Authorization` is redaction-covered.
4. **token→user→policy wiring.** Thread the authenticated principal into the t47 `McpEngine` so
   `commit`/`preview` evaluate against `server::policy::gate_plan` keyed by that user/scope;
   default-deny stands. `describe` remains pure. Irreversible `commit` still hits
   `IrreversibleGuard` (require approval until t59).
5. **Enforcement tests (hermetic).** Golden/integration tests with an in-memory store + a
   fixed signing key: no token → `401` with a usable `WWW-Authenticate`; valid token → tool call
   succeeds; expired token → `401`; out-of-policy `commit` → policy decision, not applied;
   refresh rotates and the old refresh fails. No network.
6. **Flip the bind + docs + version.** Now that the endpoint authenticates, permit a non-localhost
   bind via config (still localhost by default; proxy-injected issuer/host per decision F).
   Update the roadmap §2.2 status to **shipped** for the full handshake and update the
   skill/README to state Claude can connect over MCP+OAuth — but only now that it is true.
   `cargo run -p xtask -- gen-docs --check`; patch-bump `crates/qfs/Cargo.toml`.

## Key files
- `crates/oauth/src/verify.rs` (new): `verify_access_token`. `crates/oauth/src/flow.rs` (modify):
  refresh grant + rotation.
- `crates/http/src/handler.rs`, `crates/http/src/serve.rs` (modify): bearer middleware + `401`/
  `WWW-Authenticate` for `POST /mcp`.
- `crates/mcp/src/tools.rs` (modify): accept the authenticated principal; key the policy gate.
- `crates/store/src/oauth_store.rs` (modify): refresh rotation; optional `0006_token_use`
  migration + `schema_version` bump.
- `crates/qfs/src/serve.rs` (modify): inject verification + refresh store; allow non-localhost bind.
- `crates/cmd/tests/dep_direction.rs` (verify).
- `crates/qfs/Cargo.toml` (modify): patch bump.

## Considerations
- **Safety floor, fully closed.** Every MCP tool call is now behind a verified bearer token mapped
  to a user; `commit` still runs through the SAME default-deny policy gate
  (`enforce::evaluate`/`gate_plan`) and the SAME `IrreversibleGuard` the CLI uses — auth adds a
  *principal*, it does not loosen the gate. Irreversible plans over MCP require approval until the
  selectable safety mode (t59) is in place; document that gap.
- **Token hygiene (security-first).** Bearer/refresh tokens are `Secret`-wrapped, redaction-
  covered (`Authorization` in `SENSITIVE_HEADERS`), and stored only as hashes (refresh handles).
  Verification checks `iss`/`aud`/`exp`/signature; refresh tokens are single-use with rotation so
  a leaked refresh is detectable (a replay of a rotated handle is an `invalid_grant` and a signal
  to revoke the chain). Constant-time compares on every hashed lookup.
- **`401` that teaches the client.** The unauthenticated/expired response carries
  `WWW-Authenticate: Bearer` with the PRM pointer (t48) so a spec-compliant MCP client
  re-discovers the AS and re-authorizes without bespoke qfs knowledge — the whole point of
  decision C ("no qfs-specific auth to learn").
- **Now safe off localhost (decision F).** Enforced auth is the precondition for binding beyond
  localhost. The issuer/audience must match what the client sees *through* the trusted reverse
  proxy (the t48 proxy-aware-issuer constraint) — verify this in an integration test that fakes a
  proxy-rewritten host.
- **Dep-direction.** No new crate. The middleware reaches token verification through an injected
  consumer-side trait, not a direct `crates/http → crates/oauth` source dep where the dep guard
  forbids it; live wiring lands on `crates/qfs`. Tokio stays at the binding.
- **Open product decisions to flag (do not guess).** (a) Token revocation surface — RFC 7009
  `POST /revoke` and/or a `/sys/*` admin path (t53) to kill a session; pick one, note it. (b)
  Access-token lifetime vs. refresh lifetime + idle vs. absolute (coordinate with t46/t49). (c)
  Whether scopes gate tools coarsely (e.g. a read-only client cannot reach `commit`) in addition
  to policy — recommend yes, as defense in depth.
- **Versioning.** One PR, one patch bump, a `v0.0.x` tag on ship.
