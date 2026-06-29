---
created_at: 2026-06-29T11:11:40+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash:
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md]
---

# Fix the SKILL.md files — the embedded one steers an AI agent into unknown_source errors

## Overview

Two SKILL files document the AI operating procedure: `packages/qfs/crates/skill/assets/SKILL.md` (the
one SHIPPED in the binary, printed by `qfs skill`) and `plugins/qfs/skills/qfs/SKILL.md` (the Claude
Code plugin skill). The **embedded one is BROKEN** — an agent literally follows its worked examples,
and four of them error at the PREVIEW step the SKILL says they pass. This is the highest-severity doc
bug because an AI will obey it and fail. The plugin copy is **more honest** and should be the model.

## Exact seams (verified, fresh user)

**`crates/skill/assets/SKILL.md` (BROKEN):**
1. mail send (lines 99-104) — `/mail/drafts |> call mail.send`, claimed "PREVIEW shows irreversible:true"
   → `unknown_source: no read driver registered for source 'mail'`.
2. github merge (lines 128-132) — `/github/acme/web/pulls/42 |> call github.merge(method => 'squash')`,
   claimed "one irreversible CALL node" → `no read driver registered for source 'github'`.
3. sql read (lines 154-158) — `/sql/pg/orders |> where total > 100 |> select id, total`, claimed
   "PREVIEW is the query itself" → `unknown source 'sql'`.
4. git temporal read (lines 164-168) — `/git/myrepo@v1.0/README.md`, claimed "the @v1.0 read is pure"
   → `unknown source 'git'`.
5. drive `cp` (lines 113-118) — `cp /local/report.pdf /drive/my/Reports/` → `parse_error` (`cp` is NOT
   a one-shot grammar verb; only an interactive-shell builtin). The golden corpus actually pins the
   lowered `upsert into /drive/my/Reports/report.pdf values (…)`, not `cp`. The plugin SKILL correctly
   says cp/mv/rm are interactive-shell-only — the embedded one contradicts it.
6. "golden corpus pins each statement's PREVIEW with no network" (lines 85-87) is true only because the
   corpus registers IN-TEST fixture drivers; the shipped binary has none, so the same statements error.
7. line 78 "DESCRIBE/PREVIEW stay pure … never from a preview" implies preview always succeeds; in fact
   it errors `unknown_source` *before* any sign-in/consent concern.

**`plugins/qfs/skills/qfs/SKILL.md` (mostly fine, more honest):** lines 147-150 correctly warn that a
`capability` error on a read "often just means no account/backend is connected yet"; lines 64-65
correctly scope cp/mv/rm to interactive-shell-only. MINOR: line 91 `upsert into /s3/backups/db.sql
values (…)` → `unsupported_verb` (S3 path resolves with empty supported-verb set) — fix the example.

## Implementation steps

1. **Rewrite the embedded `SKILL.md`'s worked examples** to forms the shipped binary actually previews:
   bare `insert/update into /path …` write-plans preview; `/local`+`/sys` reads run. Mark mail/github/
   sql/git reads and `|> call` as **"needs a wired read driver / connection — not runnable from a bare
   binary"** (an agent must not be steered into `unknown_source`).
2. **Remove the one-shot `cp` example** (it doesn't parse one-shot) — use the lowered `upsert into
   /drive/… values (…)` the golden corpus actually pins; align with the plugin SKILL's "cp/mv/rm =
   interactive shell only".
3. **Soften the corpus/preview-purity claims** (lines 78, 85-87) to match the fresh-binary reality.
4. **Plugin SKILL:** fix the `/s3` upsert example (`unsupported_verb`); otherwise keep it as the honest
   template the embedded one should match. Keep the two consistent.
5. **gen-docs note:** `crates/skill/assets/SKILL.md` is shipped via `qfs skill` (not a gen-docs golden)
   — it is hand-edited here, but verify the skill golden-corpus tests still pass after the rewrite.

## Key files

- `packages/qfs/crates/skill/assets/SKILL.md` (rewrite the worked examples) +
  `packages/qfs/crates/skill/tests/golden_corpus.rs` (keep green).
- `plugins/qfs/skills/qfs/SKILL.md` (fix the s3 example; keep as the honest model).

## Considerations

- An AI agent is the primary consumer of `qfs skill` — a procedure that produces capability errors at
  step 3 is actively harmful (the agent will retry, escalate, or give up). This is the most important
  page to make honest.
- Cross-reference the foundation runnable-surface truth; the two SKILLs must AGREE with each other and
  with the binary.
