---
created_at: 2026-06-30T20:30:40+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 4h
commit_hash: a636c03
category: Added
depends_on: [20260630203030-google-live-verification-token-import.md]
---

# Guidance doc: gmail-ftp → qfs and gdrive-ftp → qfs (provably true)

Part of EPIC `20260630203000`. The headline deliverable: **by following the docs alone, the owner
reproduces the gmail-ftp / gdrive-ftp experience.** Every shown command must run against a real
account (depends on the live verification `20260630203030`, so claims are verified, not aspirational).

## Contents

1. **One-time host setup.** Create a Google Cloud "Desktop" OAuth client → download `credentials.json`
   → `cat credentials.json | qfs connection add google-app default` (qfs owns it in its DB). Same
   Google Cloud steps as gmail-ftp's README.
2. **Connecting-user auth.** `qfs identity signup <email>` → consent (`QFS_GOOGLE_CONSENT=1 qfs
   connection add gmail work`, browser/SSH-forward) → `qfs connection use google <email>`. The
   per-user refresh token is stored encrypted (needs `QFS_PASSPHRASE`).
3. **Command mapping tables** (the heart):
   - gmail-ftp `ls`/`cd`/`get`/`put`/`compose`/`send`/`mkdir`/`rm` → the qfs `/mail/...` queries.
   - gdrive-ftp `ls`/`cd`/`get`/`put`/`mkdir`/`rm` → the qfs `/drive/...` queries.
   Mark PREVIEW→`--commit`, and `send`/permanent-delete as irreversible (`--commit-irreversible`).
4. **Interactive shell** parity (`qfs` opens the FTP-like shell; `ls`/`cd` map to path navigation).

## Key files

- New `docs/guide/replace-gmail-gdrive-ftp.md` (or a pair); link from `docs/.vitepress/config.mts`
  nav/sidebar + `docs/guide/connect.md`. Reference: `~/projects/{gmail,gdrive}-ftp/README.md`.

## Considerations

- "Make docs true": do NOT document a command until it is verified live. Gaps (`20260630203010`,
  `20260630203020`) should land first, or the doc must mark them as not-yet.
- This supersedes the Google sections of `docs/guide/connect.md` — keep them consistent.
