---
created_at: 2026-06-30T20:31:40+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash:
category: Added
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
