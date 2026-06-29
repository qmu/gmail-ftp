---
created_at: 2026-06-29T11:10:10+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash: 1c30270
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140000-wire-local-single-file-content-read.md, 20260629140010-wire-codec-execution-decode-encode.md]
---

# Fix docs/guide/getting-started.md — its "follow along offline" premise is false

## Overview

`docs/guide/getting-started.md` is the first-run walkthrough. Its top claim (lines 3-5) — *"Everything
here except the final `--commit` works **offline with no credentials**, so you can follow along
immediately"* — is **false** when run against the `0.0.10` binary as a fresh user. Severity: **BROKEN**
(the page's whole premise fails on its own primary examples). It leads with `/mail`, a cloud driver
whose reads are unwired, instead of `/local`, which actually runs offline.

## Exact seams (verified against the binary, fresh user)

1. **Section 2+3 read preview ERRORS** (lines 73-74): `qfs run "/mail/inbox |> where subject LIKE
   '%invoice%' |> select date, from, subject"` →
   `{"error":{"code":"unknown_source","kind":"capability","message":"no read driver registered for source 'mail'"}}`
   (exit 3). The doc says "A read query previews as the query itself (reads change nothing)" — it does
   not; it errors.
2. **Section 4 irreversible example ERRORS even as a pure preview** (line 92): `/mail/drafts |> call
   mail.send` → same `no read driver registered for source 'mail'` — the `/mail/drafts` read leg
   fails before the CALL, so the irreversible example can never even preview.
3. **The `describe` block doesn't match the binary** (lines 26-42): real `describe /mail/drafts`
   columns are `id, thread_id, date, from, subject, snippet, label_ids, attachments` (the doc shows
   `id, date, from, subject, …` and omits `thread_id`/`snippet`/`label_ids`/`attachments`). Also the
   doc presents the human table as the plain output, but the **default when piped/non-TTY is raw
   JSON** — the table only renders on a TTY or with `--format table`.
4. **WARN noise** (foundation seam #2): every `qfs run` here prints the `github`/`slack` sign-in
   WARNs, unexplained.

## Implementation steps

1. **Lead with `/local`** (verified to run offline): make "your first queries" use a real local
   directory — `qfs describe /local/<dir>`, `qfs run "/local/<dir> |> select name, size, is_dir"`
   (a real read that returns rows), and a `/local` write-plan preview — so a fresh user genuinely
   follows along. Keep `/mail` as a *later* "connecting a real service" illustration, clearly marked
   "needs a connection / the read facet is not yet wired".
2. **Fix or remove the broken read + `call` examples** — replace the `/mail/inbox` read and the
   `/mail/drafts |> call mail.send` preview with forms that actually preview (a bare `insert into
   /mail/drafts …` previews; a `/local` read returns rows), or move them under an explicit
   "not-yet-runnable" seam note per the foundation decision.
3. **Regenerate the `describe` block** from the real binary output and note the TTY-vs-piped default
   (`--format table` for the human table).
4. Add a one-line note that the `github`/`slack` WARNs are harmless until the binary stops emitting
   them (cross-ref the foundation binary-bug ticket).

## Key files

- `docs/guide/getting-started.md` (rewrite).
- Reference: `crates/qfs/src/shell.rs` (which mounts `/local` reads), `crates/cmd/src/lib.rs`.

## Considerations

- The honest, runnable first example is the point — a walkthrough that errors on step 2 is worse than
  none. `/local` + `describe` + write-plan-preview are the offline-true surface (foundation ticket).
- Do not over-correct into claiming `/local` reads file *contents* — they return dir/stat rows only
  (foundation seam #3).
