---
created_at: 2026-06-29T11:10:50+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 0.5h
commit_hash: 25b70fa
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140000-wire-local-single-file-content-read.md, 20260629140010-wire-codec-execution-decode-encode.md]
---

# Fix docs/index.md — both headline "See it" examples don't do what they claim

## Overview

`docs/index.md` is the VitePress home page (the docs front door). Its two flagship "See it" examples
**don't work** against the binary. Severity: **MISLEADING**.

## Exact seams (verified, fresh user)

1. **"Turn a JSON file into a YAML file" does NOT transcode** (lines 49-53): `/local/config.json |>
   decode json |> encode yaml` returns the file's **stat-metadata row**, not YAML — the codec stages
   are silent no-ops (foundation seam #3). The headline produces no YAML.
2. **The cross-service join errors** (lines 41-45): `/sql/pg/orders |> join /github/acme/web/issues
   on id == issue_id |> select id, title` → `unknown source 'sql'` — the flagship "join a database to
   GitHub" pitch fails at plan time (`/sql` unwired).
3. **The mail "See it" example would error** (lines 32-37): `/mail/inbox |> where … |> select …` →
   `no read driver registered for source 'mail'`.
4. **(Accurate)** the `create trigger … do insert …` example (lines 57-61) previews cleanly
   (`is_pure:true`) — keep it.

## Implementation steps

1. **Replace the two broken "See it" examples** with ones that actually run today (a `/local` read
   that returns rows; the working `create trigger` preview; a `describe`), OR present them as
   "what qfs is *for*" with an honest "not yet runnable — driver/codec not wired" caption (foundation
   decision). Do not show JSON→YAML or the cross-service join as if they execute.
2. Keep the three-faces framing and the trigger example (verified true).

## Key files

- `docs/index.md` (edit). Reference: foundation "what runs today" note.

## Considerations

- The landing page sets the credibility bar — a hero example that silently returns wrong output
  (the JSON→YAML no-op) is the most damaging case. Prefer a humble runnable example over an
  impressive broken one until the codecs/cloud-reads are wired.
