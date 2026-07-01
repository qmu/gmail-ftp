---
created_at: 2026-06-29T11:11:10+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 0.1h
commit_hash: 25b70fa
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140110-fix-warn-noise-unrelated-cloud-drivers.md]
---

# Fix docs/guide/installation.md — note the harmless WARN noise on the first-check preview

## Overview

`docs/guide/installation.md` is **accurate and runnable** — every install-script claim was verified
against `install.sh` (the `~/.local/bin` default, the `QFS_INSTALL_DIR` override, sha256-before-extract,
the Linux-musl + macOS targets) and the first-check `describe`/`run` examples work offline. Severity:
**MINOR** — only one inherited cosmetic gap.

## Exact seams (verified)

1. **Unexplained WARN noise on the first-check preview** (line 62): `qfs run "insert into /mail/drafts
   values (…)"` previews cleanly but dumps two stderr lines `WARN qfs::consent: cloud driver
   'github'/'slack' … requires sign-in` (foundation seam #2). To a first-time user immediately after
   install, this reads like a credential problem on a command the page calls "completely offline".

## Implementation steps

1. Add a one-line note that those `github`/`slack` WARNs are harmless for an unauthenticated preview
   (and cross-reference the foundation binary-bug ticket that will stop the binary emitting them for
   unrelated drivers). Or, once the binary is fixed, drop the note.

## Key files

- `docs/guide/installation.md` (one-line note). Reference: foundation ticket seam #2.

## Considerations

- This page is otherwise correct — do not churn it. Trivial; could be folded into the foundation
  ticket if the binary WARN fix lands first.
