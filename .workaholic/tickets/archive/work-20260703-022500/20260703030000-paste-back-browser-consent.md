---
created_at: 2026-07-03T03:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort: 4h
commit_hash: 604321c
category: Changed
depends_on: []
---

# Paste-back browser consent: port gmail-ftp's terminal OAuth flow to `qfs account add google`

First-user finding (owner, v0.0.15 era). The owner's environment is SSH-to-EC2, and the
gmail-ftp consent experience is the explicitly wanted one ("that's what Claude Code offers"):
print the URL, press `c` to copy, authorize in the LOCAL browser, then **paste the redirected
`http://127.0.0.1?...` URL (or just the `code=` value) back into the terminal**. No listener, no
SSH port-forward.

qfs's current flow (`qfs_google_auth::authorize`, `crates/google-auth/src/authorize.rs:67`)
binds a REAL loopback TcpListener on the host and waits for the redirect to arrive over HTTP —
which can never happen over plain SSH (the local browser's redirect goes to the user's own
127.0.0.1). Today the only alternatives are an `ssh -L` port-forward or the stdin token import,
and the import cannot serve `/drive` (the gmail-ftp token lacks the Drive scope; one
`google:<email>:refresh_token` slot per account, so importing the gdrive-ftp token would
overwrite the Gmail one).

## The flow to implement (mirror gmail-ftp `internal/auth/auth.go`)

1. Build the auth URL with the **scope union** (existing `all_google_scopes`) and a CSRF `state`,
   with `redirect_uri=http://127.0.0.1` (no port, no listener — the paste-back convention).
2. Print the URL; offer `c` = copy via OSC 52 (works through SSH + tmux), `o` = try `xdg-open`/
   `open` (best-effort).
3. Prompt: "paste the redirected URL (or the code= value):" — read ONE line from the
   controlling terminal (`/dev/tty`-safe echoed prompt; stdin may be a pipe — reuse the
   20260703021500 gate work), accept either the full `http://127.0.0.1/?state=…&code=…` URL or a
   bare code; **verify `state`** before the exchange.
4. Exchange the code, fetch the profile email, persist under `refresh_token_key(email)`, record
   the per-driver consents keyed by that email (existing `add_google` bookkeeping).
5. Keep the loopback-listener flow as the fallback when a redirect actually can arrive (a
   desktop host); pick paste-back whenever the listener path is not confirmed working —
   simplest: make paste-back THE flow (gmail-ftp ships only this and it works everywhere).

## Key files

- `packages/qfs/crates/google-auth/src/authorize.rs` (the flow), `oauth.rs` (`build_auth_url`,
  `redirect_uri`), `crates/qfs/src/google.rs::run_google_consent` (the binary seam)
- `packages/qfs/crates/qfs/src/account.rs::add_google` (TTY branch), `crates/qfs/src/tty.rs`
  (echoed /dev/tty line-read helper)
- Reference: `~/projects/gmail-ftp/internal/auth/auth.go` (URL print, `c`/OSC 52, paste parse,
  state check)

## Considerations

- The code exchange for a Desktop-app client with `redirect_uri=http://127.0.0.1` must send the
  SAME redirect_uri in the token request (Google checks equality, not reachability) — this is
  exactly what gmail-ftp does and it is verified working with the owner's OAuth app.
- Hermetic tests: parse the pasted input (full URL / bare code / wrong state rejected); the live
  round-trip is the owner-attended verification (same as gmail-ftp's).
- Docs: getting-started + gmail/gdrive cookbooks currently say "browser consent on a TTY" —
  update the wording to describe the paste-back steps once true.

## Quality Gate

- Over plain SSH (no port-forward): `qfs account add google` prints the URL, accepts the pasted
  redirect URL, verifies state, seals the union-scope refresh token, records consents — and a
  subsequent `qfs connect /drive --driver gdrive --account <email>` read returns real rows
  (the union-scope proof gmail-ftp's token could never give).
- Wrong/missing `state` in the pasted URL is rejected before any exchange.
- Workspace tests / clippy / fmt / gen-docs / gen-skills green.
