---
created_at: 2026-06-30T01:00:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash:
category: Added
depends_on: []
---

# Git time-travel reads: honor `@<ref>` for tree/blob reads (`/git/app@v1.2/…`)

Roadmap "Near-term backlog": reading a repo's history and its **latest** tree works, but reading the
tree (or a single file's contents) **as of an older commit or tag** (`/git/app@v1.2/…`) shows the
latest version instead. The git driver already does time-travel correctly — the `@<ref>` coordinate
is **dropped in core path-lowering** before it reaches the driver.

## Root cause (confirmed — the fix is narrow and upstream of the driver)

The `@version` is parsed and preserved on the AST, then thrown away during lowering:

- `crates/core/src/resolve.rs:682-694` `render_mount_path` — comment admits *"`@version` / globs are
  addressing concerns dropped here"*; the loop pushes only `seg.name`, never `seg.version`.
- `crates/core/src/eval.rs:527-533` (execution lowering) maps `segments.iter().map(|s| s.name…)` —
  `s.version` dropped — then `render_path` (`eval.rs:966`) joins only names. The `scan.path` handed to
  the read facet has no `@ref`.
- Downstream defaults a missing `@` to HEAD: `crates/driver-git/src/path.rs:103-106` → always latest.

What already works (so don't touch it): the parser carries `version`
(`crates/parser/src/grammar.rs:291-297`, test `path_at_version_is_preserved`), and the driver fully
resolves any ref — `crates/driver-git/src/repo.rs:132 resolve_ref` (branch/tag/40-hex-sha/`HEAD~n`/
annotated-tag peel) + `blobfs.rs:19 ls` / `:44 cat` already read at the given ref; the facet even
passes `r = gp.reference` through (`read_facets.rs:266,296-297`).

## Plan

1. Re-emit `@<version>` onto the path string in `crates/core/src/resolve.rs:684 render_mount_path`.
2. Thread `seg.version` through `crates/core/src/eval.rs:526-533` + `:966 render_path` so the rendered
   scan path is `/git/<repo>@<ref>/<rest>` and `GitPath::parse`'s `split_once('@')` re-reads it.
3. Add an end-to-end test reading a tag/older-commit tree and a single blob at that ref.

## Considerations

- `resolve.rs`/`eval.rs` are **shared across all mounts** — confirm a stray `@` on a non-git path
  doesn't break other drivers' path parsers; `@` is only meaningful on the git repo segment.
- No driver change needed. Bump the patch in `crates/qfs/Cargo.toml`; consider a git time-travel
  recipe in the cookbook ratchet.
