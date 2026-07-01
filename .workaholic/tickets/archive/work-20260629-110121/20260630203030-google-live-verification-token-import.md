---
created_at: 2026-06-30T20:30:30+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, UX]
effort: 4h
commit_hash: 997b27a
category: Changed
depends_on: [20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md]
---

# Live Google verification + refresh-token import (the proof qfs replaces the FTP tools)

Part of EPIC `20260630203000`. Run qfs against the owner's REAL Google account so the guidance doc
(`20260630203040`) is provably true.

## What's in place

- App creds in qfs DB (commit `92c6a9a`): `qfs connection add google-app default` (verified).
- Token source reads `google:<email>:refresh_token` (`qfs_google_auth::refresh_token_key(email)` =
  `CredentialKey(DriverId("google"), encode_account_email(email))`).
- Active account: `QFS_GOOGLE_ACCOUNT=<email>` or `qfs connection use google <email>`.

## Blockers to resolve (the reason this is its own ticket)

1. **Account email.** gmail-ftp's `~/.config/gmail-ftp/token.json` has **no email field** (keys:
   access_token/token_type/refresh_token/expiry/expires_in). To import the existing refresh token,
   either (a) get the email from the owner, or (b) fetch the profile email from Google using the
   token, then store the refresh token under `refresh_token_key(email)`.
2. **A clean import path.** The default `qfs connection add gmail <name>` stdin path stores under
   `cred_key(gmail,<name>)`, NOT `google:<email>:refresh_token` — confirm/relate the keys, or add a
   small `qfs connection import-google-token <email>` helper that stores the existing refresh token
   under the right key. (Owner approved reusing `~/.config/{gmail,gdrive}-ftp/{credentials,token}.json`
   for now; the long-term path is a fresh qfs consent.)
3. **Headless consent.** This is a server — the loopback browser flow (`QFS_GOOGLE_CONSENT=1 qfs
   connection add gmail work`) needs an SSH port-forward. Document the SSH-forward recipe (gmail-ftp's
   README has the same "works over SSH" note).

## Acceptance

`/mail/INBOX |> select date, from, subject |> limit 5` returns real messages, and `/drive/my |>
select name` lists real Drive entries, on this host (against a fresh `XDG_CONFIG_HOME` until the
project.db blocker `20260630203120` is fixed).

## Considerations

- Never log/print the token; it is a `qfs_secrets::Secret`. Reuse of the owner's real token is opt-in
  and host-local.
