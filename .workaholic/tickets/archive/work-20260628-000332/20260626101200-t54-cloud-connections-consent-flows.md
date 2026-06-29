---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: M
commit_hash: f19ee34
category: Added
depends_on: [20260626100200-t44-accounts-to-connections-rename.md, 20260626100700-t49-oauth-dcr-authcode-pkce.md]
---

# t54 — Cloud `connections` consent (OAuth) for Drive/GitHub/Gmail; sign-in mandatory for cloud drivers

## Overview

Delivers milestone **M4** (roadmap Part 6) — the "Cloud tier" — wiring the interactive OAuth
consent flow into `connection add` for cloud drivers so a human grants Drive/GitHub/Gmail access
once and the refresh token is persisted envelope-encrypted (decision E). It implements the
local half of the roadmap's §3.1 identity story for cloud usage: **a cloud connection cannot be
used until the operator has signed in to qfs identity** (decisions B, C; M4 makes "Local + Cloud
usage" work). The OAuth *client* substrate already exists as a library: `crates/google-auth`
ships `OAuthClient`, `authorize()` (native loopback), `StoredTokenSource`, and `GoogleApiClient`,
and `driver-gmail`/`driver-gdrive`/`driver-github` already read/apply against live services. What
is genuinely **new** here is (a) routing the consent flow through the renamed `connection add`
verb (t44), (b) an analogous GitHub OAuth/device flow modelled on the Google one, (c) the
sign-in gate that refuses cloud connections for an unauthenticated operator, and (d) live
verification that the stored token actually drives a real read.

## Exact seams

- `crates/google-auth/src/lib.rs` — reuse `OAuthClient` (`build_auth_url` with
  `access_type=offline`+`prompt=consent`, `exchange_code`, `refresh_access_token`,
  `fetch_profile_email`), `authorize()` (loopback `127.0.0.1:0`, advertises
  `http://localhost:<port>`, persists `google:<email>:refresh_token`), `StoredTokenSource`,
  `GoogleApiClient` (Bearer inject, refresh-on-401). Tokens are `qfs_secrets::Secret`; network
  rides the runtime-free `HttpExchange` seam. The GitHub flow is a **new** sibling modelled on
  this — do NOT add a heavy vendor SDK.
- `crates/secrets/src/store.rs` `trait Secrets { get/put/remove/list }` — the credential seam
  the refresh token lands through; after t43 the default backend is the envelope-encrypted
  SQLite Project DB store, so consent writes the refresh token there, not the old file vault.
- `crates/secrets/src/key.rs` — `ConnectionId`/`CredentialKey { driver, account }`/
  `AccountRecord` (renamed from `Account*` by t44); the per-connection key namespace
  (`google:<email>:refresh_token`, and a new `github:<login>:refresh_token`).
- `crates/cmd/src/lib.rs` — `enum AccountVerb { Add, List, Use, Remove }` / `enum AccountAction`
  / injected `AccountLauncher` (renamed to the `connection` surface by t44). `connection add`
  for a cloud driver must dispatch into the consent flow rather than prompting for a raw secret.
- Binary wiring: `crates/qfs/src/account.rs` (`run_account`, `open_store_for_commit`,
  `active_account`) and `crates/qfs/src/commit.rs` (`networked_credential`, `live_registry()`)
  — the composition root where the consent flow + sign-in gate are injected (one of the 7
  closures `crates/qfs/src/main.rs` passes to `qfs_cmd::run(...)`).
- Identity/session seams from t45/t46 (new `qfs-identity` crate, System-DB `users`/`accounts`,
  session tokens) — the "is the operator signed in?" check the gate consults.
- `crates/qfs/src/transport.rs` — the ONE real `HttpTransport` that backs the live profile/read
  verification step.
- Dep guard `crates/cmd/tests/dep_direction.rs` — any new `qfs-driver-*`/auth edge lands on the
  binary leaf `crates/qfs`; `qfs-cmd` stays free of driver/auth crates.

## Implementation steps

1. **Sign-in gate (pure decision, then wiring).** Add a small pure predicate
   "is this driver a cloud driver requiring sign-in?" (a static set keyed by `DriverId`:
   gmail, gdrive, github, ga, slack, objstore, cf) and a gate that, in `connection add`/`use`
   for such a driver, requires an authenticated identity (t45/t46). Unit-test the predicate and
   gate decision with no I/O. Tree stays green.
2. **Route Google `connection add` through `authorize()`.** In `crates/qfs/src/account.rs`,
   when `connection add gmail|gdrive|ga <name>` runs, invoke `crates/google-auth` `authorize()`
   with the driver's minimum scope set, capture the profile email via `fetch_profile_email`,
   and persist the refresh token through the (t43) `Secrets` store under
   `google:<email>:refresh_token`. No new keyword, CLI-only.
3. **Add the GitHub consent flow.** Add a `github` auth path in (or beside) `crates/google-auth`
   's pattern — a new module mirroring `OAuthClient`/`authorize()`/`StoredTokenSource` for
   GitHub's OAuth (loopback or device-code), persisting `github:<login>:refresh_token`. Mock-HTTP
   golden tests for auth-URL shape, code exchange, and refresh — no live creds.
4. **Verify-on-add.** After consent, do one live read (e.g. `GoogleApiClient` profile call /
   GitHub `/user`) over `crates/qfs/src/transport.rs` to confirm the stored token actually
   works, and surface a typed error (re-`authorize` hint) on `invalid_grant`. Behind the
   networked-credential gate so hermetic tests never touch the network.
5. **Honest docs + skill + version.** Update `crates/skill/assets/SKILL.md` and the cookbook to
   show `qfs connection add <cloud-driver>` opening a consent flow and the sign-in requirement —
   only after the slice ships. Bump the patch in `crates/qfs/Cargo.toml` and run
   `cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/google-auth/src/lib.rs` (reuse) and a **new** `crates/google-auth/src/github.rs`
  (or a new `crates/github-auth` leaf added to the dep_direction allowlists) for the GitHub flow.
- `crates/qfs/src/account.rs` — dispatch cloud `connection add` into consent; sign-in gate.
- `crates/qfs/src/commit.rs` — `networked_credential`/`live_registry()` resolve per-connection
  cloud tokens for the verify step.
- `crates/cmd/src/lib.rs` — the `connection` verb surface forwards cloud drivers to the launcher.
- `crates/secrets/src/key.rs` — the `github:<login>:refresh_token` key shape.
- `crates/skill/assets/SKILL.md`, `docs/guide/*`, `docs/cookbook/*` — honest consent UX docs.

## Considerations

- **Safety floor.** `connection add` performs network I/O for consent only; it constructs no
  effect-plan and touches no service data — describe stays pure, preview untouched, and the
  read-verification step is a pure read, not a commit. The consent flow itself must never be
  triggered implicitly by a `preview`.
- **Secrets discipline.** Refresh tokens are `qfs_secrets::Secret` (redacted/zeroized) and land
  through the t43 envelope-encrypted store — never the scrapped file vault, never a log line,
  never an error `Display`. The loopback-host gotcha from the original Google work still holds:
  advertise `http://localhost:<port>`, bind `127.0.0.1`.
- **Sign-in mandatory (the load-bearing M4 rule).** A cloud connection must be unusable for an
  unauthenticated operator — fail closed. This is a deliberate behavior change from today's
  "any local user can add a connection"; gate it on the t45/t46 identity, and flag the
  single-user-laptop UX (does a solo local user still need to sign in?) as an **open product
  decision** rather than guessing — the roadmap's §3.1 talks about teams, not the solo case.
- **Dep-direction.** New auth/driver edges and any new auth leaf crate land on `crates/qfs` and
  the allowlists in `crates/cmd/tests/dep_direction.rs`; keep `qfs-cmd` free of driver/auth deps.
- **wasm/native.** `authorize()`'s loopback path is native-only; on Workers the refresh token is
  provisioned out-of-band and only `StoredTokenSource` runs — keep the consent path feature-gated
  so the refresh-only path still builds wasm.
- **Honesty + versioning.** Do not advertise GitHub consent until step 3 ships; one PR + patch
  bump in `crates/qfs/Cargo.toml` + a `v0.0.x` tag on ship.
