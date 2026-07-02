---
created_at: 2026-06-30T20:30:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
commit_hash:
category: Added
depends_on: []
---

# EPIC: Replace gmail-ftp / gdrive-ftp on this server with qfs

## The goal (owner, 2026-06-30)

The server runs **gmail-ftp** and **gdrive-ftp** (Go, FTP-style shells over Gmail/Drive, installed
as Claude plugins — `~/.claude/settings.json`: `gmail-ftp@gmail-ftp`, `gdrive-ftp@gdrive-ftp`, from
`qmu/gmail-ftp` / `qmu/gdrive-ftp`; binaries in `~/.local/bin`, creds in
`~/.config/{gmail,gdrive}-ftp/`). qfs (which **started as gmail-ftp**) must replace both: do the same
things, and ship **guidance docs good enough that following them ALONE reproduces the gmail-ftp /
gdrive-ftp + Claude-plugin experience**.

## Parity baseline (already true)

The drivers model the full FTP command set with the SAME OAuth scopes (gmail.modify+compose; full
Drive). Gmail: read / drafts / send (irreversible) / trash / label-modify. Drive: list / upload /
trash / mkdir / cp / mv. Gmail reads + Drive folder listing are wired live (this branch).

## Already shipped this cycle (branch work-20260629-110121)

- **Google app creds in qfs's own DB** (commit `92c6a9a`): `cat credentials.json | qfs app add
  google` (the ADR 0008 verb for the app-credential layer); `crate::google::google_app_config()`
  reads the DB first, env fallback. The owner's design point ("qfs user offers the credential, qfs
  stores it in db"). Live-proven: the app credentials.json is in qfs's DB.

## Sub-tickets

1. `20260630203010` — Gmail FTP parity gaps (`ls /` label listing + message `get`/download).
2. `20260630203020` — Drive FTP parity gaps (file content `get`/download wired into the read facet;
   verify put/mkdir/rm/cp/mv live).
3. `20260630203030` — Live Google verification + token import (account-email keying; out-of-band
   token import; headless consent over SSH).
4. `20260630203040` — Guidance doc: gmail-ftp→qfs and gdrive-ftp→qfs (provably true, run against a
   real account).
5. `20260630203050` — Package qfs as a Claude plugin / MCP that replaces gmail-ftp@/gdrive-ftp@.

## Considerations

- "Make the docs true": the guide can only claim what is verified live, so #3 (live verification)
  gates #4 (doc).
- Blocker found: this host's `~/.config/qfs/project.db` fails to open on this branch ("migration v2
  edited in place") — see ticket `20260630203120`. Verify against a throwaway `XDG_CONFIG_HOME` until
  resolved.
- Reference tools: `~/projects/gmail-ftp/README.md`, `~/projects/gdrive-ftp/README.md` (the command
  set + navigation model to match).
