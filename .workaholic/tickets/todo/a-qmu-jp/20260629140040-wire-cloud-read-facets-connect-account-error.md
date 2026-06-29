---
created_at: 2026-06-29T14:00:40+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Infrastructure]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# T5 — Cloud reads return an honest "connect your account" error, not `unknown_source`

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 3.

## Overview

`/mail`, `/drive`, `/ga`, `/s3`, `/r2` reads error `unknown_source: no read driver registered for
source 'X'` for a fresh user — which reads like an internal bug, not "you haven't connected this
account." These reads fundamentally need network + OAuth, so they can't return rows offline; the honest
fix is to register a read facet that fails with a clear, actionable `capability` error directing the
user to connect/sign in.

## Ground truth (verified 2026-06-29)

- Read lookup miss → `unknown_source` (`crates/qfs/src/exec.rs:63-65`). Read facets registered only for
  github/slack/sys/claude (`crates/qfs/src/shell.rs:241-273`); gmail/gdrive/ga/objstore have **no read facet**.
- Pattern to follow: the github/slack read-facet adapters (`crates/qfs/src/read_facets.rs:37-64`).

## Implementation steps

1. Add read-facet adapters for `mail`(gmail), `drive`(gdrive), `ga`, `s3`/`r2`(objstore) that, when no
   live client is bound, return `CfsError` `capability` with message "connect your <service> account —
   run `qfs identity signup <email>` / `qfs connection add …`" (reuse the consent error text).
2. Register them in `run_engine_and_reads()` (`crates/qfs/src/shell.rs`) so the read path resolves to a
   facet that errors clearly instead of `unknown_source`.
3. Tests: a fresh-user read of each returns the connect-account `capability` error (not `unknown_source`).

## Key files

- `crates/qfs/src/{read_facets.rs,shell.rs,consent.rs}`.

## Considerations

- Hermetic: the error path needs no network. The *rows* path is T6 (github/slack) and T7 (gmail/gdrive/ga).
- Lets the docs honestly say "these run once you connect an account" instead of hiding the failure (Phase 5,
  esp. `skill-md` so an AI agent gets an actionable error, not `unknown_source`).
