---
created_at: 2026-06-29T14:00:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category: Added
depends_on: []
---

# T3 — Wire `/git` read facet over a local repository (hermetic)

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 2. Multi-day — see sub-tasks.

## Overview

`/git/<repo>@<ref>/<path> |> select …` and `describe /git/<repo>/commits` error
(`unknown source 'git'` / `unknown_mount`): `driver-git` registers an **apply** facet but **no read
facet**, and is mounted only when configured. The driver already contains a hermetic local-object read
stack — this ticket exposes it as a read facet and registers it on the read path.

## Ground truth (verified 2026-06-29)

- `driver-git` read machinery already present: `crates/driver-git/src/{blobfs.rs,relational.rs,objectdb.rs,repo.rs,inflate.rs,sha1.rs}`.
- Only `git_apply_driver` is exported/registered (`crates/qfs/src/commit.rs:285-291`,
  mount `crates/qfs/src/shell.rs:220-222`); read facets are wired only for github/slack/sys/claude
  (`shell.rs:241-273`). Read lookup that fails: `crates/qfs/src/exec.rs:60-66`.

## Sub-tasks (each a ≤4h commit)

1. **Read facet** — add `read_rows`/read-driver entry in `driver-git` (over `blobfs`/`relational`/`objectdb`),
   mapping `@<ref>` + path → file content rows and `commits`/`refs` → relational rows.
2. **Registration** — register the git read facet in `run_engine_and_reads()` (`crates/qfs/src/shell.rs`,
   near 263-273) when a repo is configured; resolve the `unknown_mount` vs `unknown_source` split.
3. **Tests** — hermetic fixture repo: read a file at a tag, list commits, list refs; assert rows.

## Key files

- `crates/driver-git/` (new read facet), `crates/qfs/src/{shell.rs,read_facets.rs}`.

## Considerations

- Keep it hermetic (local `.git` objects only — no network fetch). Makes `code.md` git recipes and
  `concepts.md §1` git-coordinate example true (Phase 5).
- Coordinate the `content` column shape with T1/T2 so `… |> decode` works on git blobs too.
