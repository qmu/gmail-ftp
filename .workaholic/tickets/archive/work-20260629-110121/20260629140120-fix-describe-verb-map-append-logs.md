---
created_at: 2026-06-29T14:01:20+09:00
author: a@qmu.jp
type: bugfix
layer: [Domain]
effort: 2h
commit_hash: bf9406d
category: Changed
depends_on: []
---

# T9 — Fix `describe` verb map for append logs (insert/update inverted)

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 4. (Foundation binary-bug #4.)

## Overview

`describe /mail/inbox` reports `verbs.insert:false, verbs.update:true` — the **opposite** of reality. An
append log takes INSERT (append), not UPDATE; the same describe even prints `native_verbs:"SELECT(tail)
INSERT(append)"` right next to the wrong verb map, and a draft INSERT previews fine. The docs lean on
"describe always shows the exact supported set" (mail.md tip), which this contradicts.

## Ground truth (verified 2026-06-29)

- `describe /mail/inbox` →
  `"native_verbs":"SELECT(tail) INSERT(append)"` … `"verbs":{"select":true,"insert":false,"upsert":false,"update":true,"remove":true,…}`
  — `insert:false`/`update:true` is backwards for an append-log archetype.
- Source: `crates/driver-gmail/` describe schema (the verb-map construction for the append-log archetype).

## Implementation steps

1. Fix the append-log verb map so `insert:true` (append) and `update:false` (and audit `remove`/`upsert`
   against the real applier) — make `verbs` agree with `native_verbs` and the applier's actual support.
2. Add a test asserting `describe /mail/inbox` reports `insert:true, update:false`, and a guard that
   `native_verbs` and the boolean `verbs` map cannot disagree (ideally derive one from the other).
3. Audit the other append-log / cloud describes for the same inversion.

## Key files

- `crates/driver-gmail/` (describe schema), and the shared verb-map builder if one exists.

## Considerations

- Restores trust in `describe` (the safety-model cornerstone an AI agent relies on). Unblocks the mail.md
  "describe shows the exact supported set" tip (Phase 5, ticket `111120`/`111140`).
