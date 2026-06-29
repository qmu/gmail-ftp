---
created_at: 2026-06-29T14:01:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category: Added
depends_on: [20260629140040-wire-cloud-read-facets-connect-account-error.md]
---

# T7 — Implement `read_rows` for `/mail` (gmail), `/drive` (gdrive), `/ga`

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 3. Network — gated, non-hermetic. Multi-day.

## Overview

Unlike github/slack, the Google drivers have **no `read_rows` entry point** — only apply/plan paths —
so even a fully-authenticated user cannot read mail/drive/analytics. `driver-gdrive` already has
`plan_read()` + `decode_body()` to build on. This is the largest cloud-read piece and the last to land.

## Ground truth (verified 2026-06-29)

- `driver-gmail` exports only `GmailApplier` (no `read_rows`); `driver-gdrive` has `read.rs` with
  `plan_read`/`decode_body` but no `read_rows`; `driver-ga` has no read module. There is no
  `live_google_client()` reader; `live_google_stack()` (`crates/qfs/src/google.rs:173-187`) is apply-only.

## Sub-tasks (each a ≤4h commit)

1. **gdrive `read_rows`** — wrap `plan_read`/`decode_body` into a read facet returning file/listing rows;
   register in the google read path (`crates/qfs/src/shell.rs`).
2. **gmail `read_rows`** — list/search messages (inbox/drafts) → rows matching the describe schema
   (`id,thread_id,date,from,subject,snippet,label_ids,attachments`).
3. **ga `read_rows`** — report rows (lowest priority; may defer to its own ticket).
4. **Gated integration tests** behind real-credential env; default suite still asserts T5's connect-account error.

## Key files

- `crates/driver-gmail/`, `crates/driver-gdrive/`, `crates/driver-ga/`, `crates/qfs/src/{google.rs,shell.rs,read_facets.rs}`.

## Considerations

- Highest-effort, fully network-bound — schedule last. Makes `mail.md` reads and Drive recipes true for a
  connected user (Phase 5). Until then, T5 keeps the error honest and the docs mark these "connect your account".
