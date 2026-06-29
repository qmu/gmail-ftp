---
created_at: 2026-06-29T11:11:30+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 0.25h
commit_hash:
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md]
---

# Fix docs/query-cookbook.md — "now live" / "preview shows what would come back" overstates runnability

## Overview

`docs/query-cookbook.md` is the large grammar catalogue (its `grammar=core` recipes are parse-checked
by the `roadmap_cookbook.rs` ratchet). Its prose is otherwise honest (no `/roadmap` link, accurate
`core`/`extended` tags — verified: a `core` `create trigger` previews, an `extended` `let …`/lambda
parse-fails). But two lines **conflate "the grammar parses" with "the query runs"**. Severity:
**MISLEADING**.

## Exact seams (verified, fresh user)

1. **"Most of this catalogue is now live"** (line 18) + **"Everything here is read-only, so PREVIEW
   shows exactly what would come back and nothing is ever touched"** (line 36). But every cloud-driver
   READ recipe errors at run: `/sql/… → unknown source 'sql'`; `/s3/… → no read driver registered`;
   `/mail`, `/github`, `/slack`, `/git@ref` reads → `unknown_source`. Only `/local` and `/sys` reads
   return rows. The warning box hedges only "a path whose driver arrives later (`/sys`, `/hosts`,
   `/directories`)" — understating that the mainstream cloud reads are ALSO unwired. A reader expects
   rows and gets `unknown_source`.

## Implementation steps

1. **Clarify "live" = the grammar parses** (which the ratchet proves), and that **executing** most
   recipes still needs the driver wired — today only `/local` + `/sys` reads run; cloud-driver reads
   and `|> call` pipelines return `unknown_source`. Bare write-plans (`update /sql/… `, `insert into
   /github/…`) DO preview (they don't read) — note that distinction.
2. Widen the warning box to name the unwired cloud reads, not just `/sys`/`/hosts`/`/directories`.
3. Keep the recipes + grammar tags (they are accurate and ratchet-checked) — this is a prose-honesty
   fix only.

## Key files

- `docs/query-cookbook.md` (prose edit only — leave the fenced recipe blocks so the
  `roadmap_cookbook.rs` ratchet stays green).

## Considerations

- Do NOT touch the `## qfs` fenced recipes (the ratchet test extracts and parses them). This is purely
  the "live / preview shows what would come back" framing.
- The test file is named `roadmap_cookbook.rs` — a code rename is out of scope (flag only).
