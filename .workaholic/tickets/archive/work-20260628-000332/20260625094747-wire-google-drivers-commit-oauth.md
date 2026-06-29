---
created_at: 2026-06-25T09:47:47+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: M
commit_hash: 0be5e31
category: Added
depends_on: []
---

# Wire gmail / gdrive / ga live commit (Google OAuth) into the binary

## Overview

The Google production clients are **already built**: `qfs-google-auth` ships the complete OAuth
loopback consent flow, token exchange/refresh, per-account refresh-token storage, and the
authenticated `GoogleApiClient`; each driver has its real client (`GoogleApiGmailClient`,
`GoogleApiDriveClient`, `GoogleApiGaClient`) over it. They were never wired into the running
binary's commit registry, and the `account add` consent flow was never wired in — so there is no way
to obtain a token, and a `/mail`, `/drive`, or `/ga` commit never reaches a real client.

This is the github/slack pattern (shipped in v0.0.4, see the umbrella ticket) plus the OAuth
specifics. **Production clients exist — this is wiring + config + live verification.**

## Exact seams

- **Transport:** `ReqwestTransport` (`crates/qfs/src/transport.rs`) already speaks `qfs-http-core`
  DTOs. `qfs_google_auth::HttpExchange` re-exports those same DTOs (`exchange(&HttpRequest) ->
  Result<HttpResponse, TransportError>`), so add an `impl qfs_google_auth::HttpExchange for
  ReqwestTransport` (pure delegate + error remap, like the github/slack impls). Unit-test it against
  a loopback server (mirror the existing transport tests).
- **OAuth app config:** `qfs_google_auth::OAuthClient::new(client_id, client_secret: Secret,
  scopes, http)`. There is NO baked-in app — the operator must register a Google "Desktop" OAuth
  app. Source `client_id`/`client_secret` from env (decide the names, e.g.
  `QFS_GOOGLE_CLIENT_ID` / `QFS_GOOGLE_CLIENT_SECRET`) or the credential store. If absent, do NOT
  register the Google drivers (honest: a `/mail` commit then fails "no driver / not configured").
- **Account model:** the refresh token lives under `google:<email>:refresh_token`
  (`qfs_google_auth::{GOOGLE_DRIVER_ID = "google", refresh_token_key(email)}`), shared across
  gmail/gdrive/ga. Resolve the active Google **email** (decide: a `google` active selection vs.
  per-driver). `StoredTokenSource::new(email, store, oauth)` → `GoogleApiClient::new(transport,
  tokens)` → `GoogleApi{Gmail,Drive,Ga}Client::new(api)` → `{gmail,gdrive,ga}_apply_driver` →
  register in `commit.rs` `live_registry()` under DriverIds `mail`/`drive`/`ga`.
- **`account add` consent flow:** wire `qfs_google_auth::{authorize, ConsentOpener}` (the loopback
  browser consent) into `account.rs` for the Google drivers so `qfs account add gmail <name>`
  actually runs consent and stores the refresh token. Without this the commit path is unreachable.
  Per-driver scope: `GMAIL_MODIFY_SCOPE`/`GMAIL_COMPOSE_SCOPE`, `DRIVE_SCOPE`,
  `ANALYTICS_READONLY_SCOPE`.
- **Planning mounts:** register cred-free gmail/gdrive/ga mounts in `run_engine_and_reads`
  (`shell.rs`) so `/mail`, `/drive`, `/ga` statements PLAN (they already do for describe).
- **Dep direction:** add `qfs-google-auth` to the binary's `allowed` set in
  `crates/cmd/tests/dep_direction.rs` (it is a pure leaf off `qfs-secrets`/`qfs-http-core`, no
  runtime/reqwest — reqwest stays in `ReqwestTransport`).

## Verification

- Unit: the `HttpExchange` adapter against a loopback server; the consent flow with
  `qfs_google_auth::MockExchange` + a scripted code; no-credential commit fails closed.
- **Live (needs a connected env):** a real Google Desktop OAuth app + a test Google account to
  confirm `qfs account add gmail`, then a real draft/list commit. Cannot be verified offline.

## Considerations

- Secrets never logged/argv (the `Secret` type redacts; OAuth tokens are `Secret`).
- Honesty: do not document Google commit as working until the live smoke passes; until then the
  no-config path must fail closed, not fake success.
- Patch bump + docs-in-lockstep per the umbrella ticket's rules.
