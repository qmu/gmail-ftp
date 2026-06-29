---
created_at: 2026-06-29T14:00:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category: Added
depends_on: [20260629140040-wire-cloud-read-facets-connect-account-error.md]
---

# T6 — `/github` and `/slack` reads return rows end-to-end when a token is present

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 3. Network — gated, non-hermetic. Multi-day.

## Overview

`driver-github` and `driver-slack` already export `read_rows`, and their read facets are conditionally
registered — but only when `live_github_client()` / `live_slack_client()` succeed (token present + consent).
Today a fresh user gets `no read driver registered`. This ticket makes the authenticated path actually
return rows (issues, pulls, messages) and documents the connect flow.

## Ground truth (verified 2026-06-29)

- `driver-github`/`driver-slack` export `read_rows` (lib.rs public use); read facets registered at
  `crates/qfs/src/shell.rs:263-273` only if the client builder returns `Some`
  (`crates/qfs/src/clients.rs:33-60`, gated by `networked_credential()` + `cloud_bind_allowed()`).

## Sub-tasks (each a ≤4h commit)

1. **Verify the authenticated read path** end-to-end with a bound client (issues/pulls list, slack messages);
   fix any gaps between `read_rows` and the registered facet.
2. **Connect-flow docs hook** — confirm `qfs identity signup` / `qfs connection add` binds the client so
   the read facet registers; surface a precise error when scopes are missing.
3. **Gated integration test** — behind an opt-in feature/credential env (NOT in the default hermetic suite)
   that exercises a real read; default suite asserts the connect-account error from T5.

## Key files

- `crates/driver-github/`, `crates/driver-slack/`, `crates/qfs/src/{clients.rs,shell.rs}`.

## Considerations

- Lowest priority with T7 (network-bound). Makes `code.md` GitHub/Slack read recipes true for a
  connected user (Phase 5). The cross-service join (T4 SQL leg + this GitHub leg) becomes runnable for a
  connected user once both land.
