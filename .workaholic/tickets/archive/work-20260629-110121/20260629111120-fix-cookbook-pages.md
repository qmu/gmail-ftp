---
created_at: 2026-06-29T11:11:20+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 2h
commit_hash: 1c30270
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140000-wire-local-single-file-content-read.md, 20260629140010-wire-codec-execution-decode-encode.md, 20260629140020-wire-git-read-facet-local-repo.md, 20260629140030-wire-sql-read-facet-sqlite.md, 20260629140040-wire-cloud-read-facets-connect-account-error.md, 20260629140050-wire-github-slack-reads-end-to-end.md, 20260629140100-wire-gmail-gdrive-ga-read-rows.md]
---

# Fix the cookbook (docs/cookbook/*) — nearly every recipe errors for a fresh user

## Overview

All seven `docs/cookbook/*` pages were run against the `0.0.10` binary as a fresh user. The cookbook —
the "real recipes" section — is **almost entirely non-executable**: every read of mail/sql/github/
slack/s3/drive/git errors `unknown_source`, the file-format recipes return silently-wrong output, and
the automation page's own `.qfs` config does not parse. Severity: **BROKEN** (most pages). This is one
ticket covering all seven, with each page's verified breakage listed; the fix is shared (foundation
decision: demonstrate only what runs, mark seams).

## Exact seams (verified per page, fresh user)

- **`cross-service.md` — 0 of 5 runnable.** Every recipe starts `/sql/pg/… |> …` → `unknown source
  'sql'` (the two joins, the local-CSV join, the union, the jsonl snapshot). The page billed as "what
  qfs is for" is wholly non-executable.
- **`databases.md` — 2 of 12 runnable.** All 9 read/aggregate/union/except recipes → `unknown source
  'sql'`; `insert … returning id` → `unrouted_path` (a different error); only `update …` and
  `upsert …` preview. The "pushes filters down into the database" headline is unverifiable (no SQL
  driver).
- **`files.md` — codecs return wrong output SILENTLY.** `/local/<file> |> decode json |> encode yaml`
  (and `encode md`/`csv`) all return the file's **stat row**, not converted data (foundation seam #3);
  a plain `/local/<file>` read also returns one stat row (never bytes). `/s3` upsert/remove →
  `unsupported_verb` (`supported:[]`). 1 genuinely-works (`/local` dir list) + 1 preview of 8.
- **`code.md` — 5 of 7 error.** git `@<ref>` reads → `unknown source 'git'`; GitHub `pulls` +
  `call github.merge` → `no read driver registered`; slack read → `unknown source 'slack'`. Only the
  two `insert into /git/myrepo/commits …` / `insert into /slack/.../messages …` write-plans preview.
- **`mail.md` — 6 of 9 error.** `id:18f1a2b3c4` does not parse (`UNEXPECTED_TOKEN`) — invalid grammar,
  independent of mail being unwired; the 4 read recipes → `no read driver registered for source
  'mail'`; `/mail/drafts |> call mail.send` errors at the read leg. The honest parts: the draft INSERT
  and the two REMOVE-gate examples. **Also:** the page's "describe always shows the exact supported
  set" tip is contradicted by `describe /mail/inbox` reporting `update:true, insert:false` (foundation
  seam #4 — a describe bug).
- **`automation.md` — the documented `.qfs` config does not parse.** The exact block (lines 50-56, two
  adjacent `CREATE` statements separated only by a newline) → `config parse error … RESERVED_AS_IDENTIFIER`;
  it parses only with a `;` or blank line between statements (a separator the docs omit). So the
  documented `qfs serve app.qfs` / `qfs job run app.qfs nightly` / `qfs job cron app.qfs nightly` all
  fail on the page's own config. The "archive every new row" trigger → `unsupported_verb` UPSERT on
  `/s3`. Bindings "preview" but return empty (`rows:[], is_pure:true`) — they confirm parse-validity,
  not an install plan. The "embedded dashboard / approval cards / MCP endpoint" prose is unreachable
  because `serve` needs a config that can't parse.
- **`index.md` — MINOR.** The "Preview first, always" tip claims `qfs run` "shows the plan and changes
  nothing" — true for writes, but **reads error** rather than previewing; the tip oversells preview
  for the read recipes that dominate the cookbook.

## Implementation steps

1. **Per page, retype every recipe into the binary** and keep only forms that run today, OR rewrite
   them onto the runnable surface (`/local` reads, `/sys` reads, write-plan previews) with the
   not-yet-wired services clearly seam-marked (foundation decision). A cookbook of non-runnable recipes
   is worse than a short honest one.
2. **`files.md`:** remove/seam the codec "convert formats" recipes until `/local` content-read+codecs
   land; fix the `/s3` verbs claim.
3. **`automation.md`:** fix the `.qfs` examples to include the statement separator the parser requires,
   and verify `qfs job run/cron <config> <name>` against `crates/cmd/src/lib.rs`; seam-mark the
   dashboard/MCP prose (needs a running, validly-configured server).
4. **`mail.md`:** fix the invalid `id:` recipe to valid grammar; correct the describe-verbs tip.
5. **`databases.md`/`cross-service.md`/`code.md`:** mark `/sql`/`/git`/cloud reads as unwired; keep
   the write-plan previews that run.
6. **`index.md`:** correct the "preview shows the plan" tip to note reads currently error on unwired
   drivers.

## Key files

- `docs/cookbook/{index,mail,databases,files,cross-service,code,automation}.md` (all edited).
- Reference: `crates/cmd/src/lib.rs` (job verbs), the foundation runnable-surface note,
  `crates/driver-local/` (codec/no-op), the parser's multi-statement-config separator rule.

## Considerations

- This is the largest honesty gap in the docs — the cookbook's whole promise (cross-service joins,
  format conversion, automation) is the unwired part. The fix likely depends on the foundation
  decision: if the cloud reads/codecs are going to be wired soon, some recipes can stay with a
  "coming soon" mark; if not, they must be removed or rewritten onto `/local`/`/sys`.
- Each page must end the implementation with a "every fenced example runs (or is seam-marked)" grep/run
  check — the parse-coverage ratchet (`roadmap_cookbook.rs`) only covers `docs/query-cookbook.md`, not
  these pages.
