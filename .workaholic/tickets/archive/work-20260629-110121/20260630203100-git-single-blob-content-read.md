---
created_at: 2026-06-30T20:31:40+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 0.5h
commit_hash: e4f6d9e
category: Changed
depends_on: []
---

# Read a single git blob's content (`/git/<repo>/<file>`) (owner item #4)

## What's wanted

Reading a single file's **contents** from a git repo — `/git/<repo>/<file.txt>` (optionally at a
ref, `/git/<repo>@<ref>/<file.txt>`). Today only the **tree listing** + commits/refs/etc read;
`/git/tt/f.txt` errors `invalid_path` (single-blob read unwired).

## Current state

- Tree listing + time-travel `@<ref>` work (commits `c5cfa89`, `8075c77` for `@HEAD~1`).
- The driver CAN read a blob: `crates/driver-git/src/blobfs.rs::cat` reads a blob at a ref; the read
  facet (`crate::read_facets::GitReadDriver`) handles `GitNode::{Blob,Root}` via `blobfs::ls` (tree),
  but a **file** path resolves to a blob node that isn't returned as content rows.

## Plan

- In `crate::read_facets::GitReadDriver::scan`, when `GitPath::parse` yields a blob FILE (not a tree
  dir), call `blobfs::cat(repo, ref, file)` → bytes → a `content` row (so `… |> decode json` etc.
  work), mirroring the local-fs file read. Confirm `GitNode::Blob { path }` distinguishes dir vs file
  (or add the distinction in `crates/driver-git/src/path.rs`).

## Key files

- `crates/qfs/src/read_facets.rs` (`GitReadDriver`), `crates/driver-git/src/{blobfs.rs,path.rs}`.

## Considerations

- Hermetically testable with a temp git repo fixture (see `.cargo-tmp/ttgit` pattern used in the
  time-travel work). Combine with `@<ref>` so `/git/app@v1.2/README.md |> decode md` reads the file
  at that tag.

## Final Report

Development completed as planned. A blob FILE path (`/git/<repo>[@<ref>]/<file>`) now reads its
content (one row carrying a `path` + a `content` Bytes column) instead of erroring `invalid_path`;
a directory path still lists the tree. `… |> decode <fmt>` now has bytes to decode, proven live.

### Discovered Insights

- **Insight**: The git path archetype (`GitNode::Blob { path }`) cannot know file-vs-directory — that
  needs the object DB — so the dispatch must happen at read time, not in the parser. Implemented as a
  new `blobfs::read` that tries `cat` first (which matches ONLY non-tree entries via `resolve_blob`)
  and falls back to `ls`, so a tree path naturally lists and a blob path reads content.
  **Context**: This is why the ticket's "confirm `GitNode::Blob` distinguishes dir vs file" resolves
  to "it can't, and shouldn't" — keeping the parser pure and the object-aware decision in `blobfs`.
- **Insight**: `DECODE` (`crates/exec/src/codec.rs`) requires its input batch to have exactly ONE row
  and a column literally named `content` (Bytes or Text). The git content batch reuses that exact
  column name so the existing codec stage consumes it with no codec-side change — the same contract
  the `/local/<file>` read already satisfies.
  **Context**: Any future blob-bearing read facet must emit a single-row `content` column to be
  `DECODE`-compatible; the name is the well-known `CONTENT_COL` constant shared across drivers.
