---
created_at: 2026-06-29T14:01:10+09:00
author: a@qmu.jp
type: bugfix
layer: [UX, Infrastructure]
effort: 4h
commit_hash: 9acb053
category: Changed
depends_on: []
---

# T8 — Stop the per-run WARN noise about cloud drivers a statement doesn't target

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 4. (Foundation binary-bug #2.)

## Overview

Every `qfs run` — even a pure `/local` ls or a `create trigger` — dumps two-plus stderr `WARN
qfs::consent: cloud driver 'github'/'slack' … requires sign-in (cloud_sign_in_required)` lines. To a
first-time user it reads like a credential failure on an unrelated command, and makes clean previews look
broken. The docs cannot honestly hide it; the binary should only warn for a driver the statement actually
targets (or downgrade to a single, opt-in hint).

## Ground truth (verified 2026-06-29)

- Warn fires in `cloud_bind_allowed()` for EVERY cloud driver at registry-build time, regardless of the
  statement's target: `crates/qfs/src/commit.rs:517-536`. Registry is built once per run
  (`shell.rs:172`, `commit.rs:26-27`), attempting to bind gmail/gdrive/ga/github/slack/objstore/cf and
  warning on each refusal (`crates/qfs/src/consent.rs:102-124`).

## Implementation steps

1. Make binding **lazy/target-scoped**: only attempt the bind (and thus only warn) for drivers the parsed
   statement references; OR downgrade the per-driver refusal to `debug!` and emit at most one consolidated
   `info` hint when a *targeted* cloud driver is unbound.
2. Preserve the fail-closed model — an actually-targeted cloud read/write must still surface the
   sign-in requirement clearly (coordinate with T5's connect-account error).
3. Tests: a `/local`-only or `/sys`-only `run` emits **zero** cloud WARNs; a `/github` read still warns/errs.

## Key files

- `crates/qfs/src/{commit.rs,consent.rs}`.

## Considerations

- Once this lands, drop the WARN note from `installation.md` and `getting-started.md` (Phase 5,
  ticket `111110`).
- Explorer flagged the current behavior as "by design (startup fail-closed visibility)" — the fix is to
  keep the visibility for *targeted* drivers only, not to remove fail-closed semantics.
