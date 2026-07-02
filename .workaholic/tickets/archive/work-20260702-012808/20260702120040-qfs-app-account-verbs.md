---
created_at: 2026-07-02T12:00:40+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
commit_hash: 83a37da
category: Added
depends_on: [20260702120030-qfs-init-one-operator.md]
---

# `qfs app` + `qfs account` — per-layer verbs for OAuth apps and service accounts

Part of EPIC `20260702120000` (ADR 0008 §3). Dissolve the three heterogeneous things the
`connection` namespace carries into their own layers: **`qfs app`** owns OAuth app registrations
(today `connection add google-app default`), **`qfs account`** owns external service accounts —
the token + the recorded consent (today `connection add google '<email>'` + `connection add gmail
default`). After this ticket, `connection add` remains only for the plain per-driver credential
case, which `20260702120050` then retires with the rest of the namespace.

## Steps

1. **`qfs app add|list|remove <provider>`** (new verb + launcher, logic beside
   `crates/qfs/src/google.rs::google_app_config` / GOOGLE_APP_DRIVER @82): `qfs app add google <
   credentials.json` stores the client id/secret in the vault (same sealed rows, new selector
   naming, e.g. `app:google:default`). `app list` shows providers only — never secrets. Env
   fallback (`QFS_GOOGLE_CLIENT_ID/SECRET`) unchanged for CI.
2. **`qfs account add <provider>`** — the consent flow:
   - Google, interactive: run the loopback browser consent (today's `QFS_GOOGLE_CONSENT` branch in
     `connection.rs:284` → `google::run_google_consent`), store the refresh token under
     `google:<email>:refresh_token`, and record consent — the account IS `google:<email>`.
     The `QFS_GOOGLE_CONSENT` env opt-in gate is retired: `account add` on a TTY *is* the opt-in.
   - Google, token import (agents/CI/no browser): `printf %s "$REFRESH_TOKEN" | qfs account add
     google you@example.com` — the stdin path, replacing the awkward url-encoded
     `connection add google 'you%40example.com'` + `connection add gmail default` two-step.
   - Non-Google cloud drivers (github/slack/objstore/cf): `printf %s "$TOKEN" | qfs account add
     github work` — subsumes today's `connection add github work` for cloud drivers, including the
     consent record (`db_record_consent`, keyed toward (driver, account) — coordinate with
     `20260702120050`'s bind-path change).
3. **`qfs account list|remove`** — accounts with provider + label + consent scope + created_at;
   never a token. `remove` deletes token + consent rows (data-sovereignty: deletion is first-class).
4. **Gate rewording**: ConsentError messages naming `qfs connection add gmail` now name
   `qfs account add google` (`consent.rs:49-74`; also the actionable error text embedded in driver
   read errors — grep `connection add` across crates).

## Key files

- `packages/qfs/crates/qfs/src/connection.rs` (the Add arm @263-360: google-app / google / consent
  branches move out), `src/google.rs` (`run_google_consent` @246, `google_app_config`,
  `GOOGLE_APP_DRIVER`), `src/secret_store.rs` (`db_record_consent` @371)
- `packages/qfs/crates/cmd/src/lib.rs` (new App/Account verbs + launcher aliases + dispatch tests),
  `crates/qfs/src/main.rs`
- `packages/qfs/crates/secrets/src/consent.rs` (CLOUD_DRIVERS @35, error copy)

## Considerations

- Layer vocabulary is load-bearing (terminology policy): **app** = OAuth client registration,
  **account** = external identity + token + consent. One concept, one word, everywhere including
  error copy and `/sys` surfaces.
- Keep the secret-on-stdin rule absolute; account labels/emails are argv-safe metadata.
- The one-consent-serves-gmail+gdrive+ga sharing (`google:<email>:refresh_token`,
  `all_google_scopes`, incremental auth from commit `c95162a`) is preserved — `account add google`
  is provider-level, not driver-level.
- `connection use` / selection is NOT touched here (that's `20260702120050`); during this ticket
  the bind path may still read the old selection for backward-internal wiring. Keep the overlap
  window compiling and green.

## Quality Gate

Global gate (EPIC) plus:

- Dispatch tests for `app` and `account` verbs (sentinel pattern).
- Hermetic tests: `app add google` seals + `app list` shows provider only; `account add google
  <email>` via stdin stores the refresh token under the right key + records consent; `account
  remove` deletes token AND consent rows; `account add` for a cloud driver without `qfs init`
  fails closed with the new error text.
- No ConsentError or driver-error string references `connection add` (assertion updated in
  consent tests).
- Local smoke (this machine): `qfs app add google < ~/.config/gmail-ftp/credentials.json` then
  `printf %s "$RT" | qfs account add google <email>` succeed; `qfs account list` shows the account.
