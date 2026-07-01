---
created_at: 2026-06-29T11:10:20+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash: 1c30270
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md]
---

# Fix docs/guide/shell.md — the prompt, builtins, and the preview/commit "session mode" are fabricated

## Overview

`docs/guide/shell.md` describes the interactive shell, but verified against the binary it is largely
**fabricated** — the prompt is wrong, `describe` is not a shell command, the "preview/commit session
mode" does not exist, there is no documented exit, and the example transcript fails line by line.
Severity: **BROKEN**. Ground truth (from `crates/exec/src/shell/desugar.rs` + `crates/qfs/src/shell.rs`):
the only builtins are `ls, cd, pwd, cat, cp, mv, rm`; the only control keyword is a bare `COMMIT`.

## Exact seams (verified, fresh user)

1. **Prompt is wrong.** Doc shows `qfs:/>` and `qfs:/drive/my/Reports>`. Actual is `local:/$` and e.g.
   `local:/tmp/tmp.XXX$` (format `{driver}:{path}$ `).
2. **`describe <path>` is not a shell command** (doc line 25). Typing `describe /mail/drafts` in the
   shell → `error[parse_error]: the grammar did not expect this token here — UNEXPECTED_TOKEN`
   (`describe` falls through to the pipe-SQL parser and fails). `describe` is a CLI subcommand, not a
   shell builtin.
3. **The `preview`/`commit` session-mode model is invented** (doc lines 40-46). There is no persistent
   mode toggle: `preview` → `error[parse_error] … RESERVED_AS_IDENTIFIER`; `commit` is recognized only
   as the bare `COMMIT` confirmation of the *immediately preceding* previewed effect (`nothing to
   commit` otherwise). You cannot "switch the session between previewing and committing."
4. **No documented exit.** `exit`/`quit`/`help` all error (`unknown source '(let:exit)'` etc.); the
   shell ends only on EOF (Ctrl-D) — the page never says so.
5. **The example transcript fails line by line** (lines 31-35). `cd /drive/my/Reports` →
   `error[unknown_mount]: no driver is mounted there`; `cat /sql/pg/orders` → `unknown source 'sql'`.
   Only `/local` is mounted; `cd`/`ls`/`cp` work only there.
6. **`cat <file>` does not read file contents** (line 22) — a single `/local` file read returns
   stat-listing rows (`name|path|size|…`), never the bytes (foundation seam #3).

## Implementation steps

1. **Rewrite to the real shell:** the real prompt (`local:/$`), the real builtins (`ls, cd, pwd, cat,
   cp, mv, rm`), the real `COMMIT` confirmation flow (preview prints an effect line; a bare `commit`
   applies the immediately-preceding one), and **how to exit** (Ctrl-D / EOF).
2. **Use only `/local` paths** in the transcript (the only mounted driver in the shell) so every line
   runs; drop or seam-mark `/drive`/`/sql` lines.
3. **Remove `describe` as a shell command** (or note it is a CLI subcommand, not a shell builtin).
4. **Remove the invented preview/commit session-mode section**; describe the real per-effect `COMMIT`
   confirmation instead.
5. **Fix the `cat` claim** — it lists stat rows, not file contents (until content-read lands, seam #3).

## Key files

- `docs/guide/shell.md` (rewrite).
- Reference (ground truth): `crates/exec/src/shell/desugar.rs` (builtins), `crates/qfs/src/shell.rs`
  (prompt, `COMMIT`, mounted drivers).

## Considerations

- This page is the clearest case of docs describing an imagined product. Every claim must be retyped
  into the running shell and kept only if it works.
- Cosmetic: `pwd` prints a `/local`-prefixed path while the prompt shows `local:/…` — reconcile the
  two in the rewrite.
