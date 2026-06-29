---
created_at: 2026-06-29T11:11:00+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 0.5h
commit_hash:
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140120-fix-describe-verb-map-append-logs.md]
---

# Fix docs/guide/cli.md — one broken example + small accuracy gaps vs the binary

## Overview

`docs/guide/cli.md` is mostly accurate — **every documented verb and sub-verb exists and matches
`crates/cmd/src/lib.rs`** (`run/describe/connection/identity/invite/job/skill/serve`, and
`connection {add,list,use,remove,rotate,revoke,rekey}`). A few examples/details are wrong. Severity:
**MISLEADING** (one example errors; the rest cosmetic).

## Exact seams (verified, fresh user)

1. **The irreversible `mail.send` example errors at preview** (line 50): `qfs run "/mail/drafts |>
   call mail.send" …` → even the no-commit preview fails `no read driver registered for source 'mail'`
   (exit 3). The sibling `insert into /mail/drafts …` (line 46) DOES preview — use that, or seam-mark
   the CALL example.
2. **`--version` sample is missing a line** (lines 149-153): the binary prints a 4th line
   `wasm32:  false` after `qfs 0.0.10` / `commit:` / `target:`. Add it.
3. **The Commands help-block order is wrong** (lines 10-18): doc order is run, describe, connection,
   …; actual `qfs --help` order is run, describe, **skill, serve**, connection, identity, invite, job,
   help. Match the binary (and the omitted tagline/`after_help`).

## Implementation steps

1. Replace the `mail.send` CALL example with the working `insert into /mail/drafts …` preview, or mark
   it "needs a wired mail read driver".
2. Regenerate the `--version` sample from the binary (include `wasm32:`).
3. Reorder the Commands block to match `qfs --help`.

## Key files

- `docs/guide/cli.md` (edit). Reference: `crates/cmd/src/lib.rs`, `crates/qfs/src/version.rs`,
  `qfs --help`.

## Considerations

- The verb/flag inventory is otherwise correct and complete — keep it; this is a small accuracy pass,
  not a rewrite.
