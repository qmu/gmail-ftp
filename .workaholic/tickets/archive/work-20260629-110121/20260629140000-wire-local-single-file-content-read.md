---
created_at: 2026-06-29T14:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: 4h
commit_hash: b1bc03d
category: Added
depends_on: []
---

# T1 — Wire `/local/<file>` single-file content read (emit a `content` Bytes column)

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 1.

## Overview

Reading a single local file (`/local/path/config.json`) returns a **stat row**
(`name,path,size,modified,is_dir,mode`), never the file's bytes. This is the precondition for codecs
(T2): a codec needs a `content` (Bytes) column to decode. The infra already exists — only the local
read path needs to surface bytes for a single-file resolution.

## Ground truth (verified 2026-06-29)

- `qfs run "/local/<file>.json |> decode json |> encode yaml"` → returns the stat row
  `{name,path,size,modified,is_dir,mode}` (codec is a no-op because there is no bytes column).
- Infra present: `Value::Bytes` (`crates/types/src/value.rs:84`), `ColumnType::Bytes`
  (`crates/types/src/schema.rs`), `fs_core::read_blob` (`crates/driver-local/src/fs_core.rs:376`),
  and the write side already carries blob bytes under `CONTENT_COL` (`crates/driver-local/src/effect.rs:22`).
- Read path: `crates/driver-local/src/read.rs:39-71` (`scan_rows`/`scan_local_rows`, dispatches
  glob vs dir vs single file), schema `crates/driver-local/src/row.rs:26-53`, describe
  `crates/driver-local/src/lib.rs:140`.

## Implementation steps

1. In `scan_local_rows` (`read.rs:54-71`), detect when the path resolves to a **single non-dir file**
   and read its bytes via `fs_core::read_blob` (sandbox resolve still applies — reject `..`/symlink escapes).
2. Emit a row carrying a `content: Bytes` column for that single-file case; keep directory/glob
   listings exactly as today (stat rows, no content) so existing behavior and tests are unchanged.
3. Make `describe()` schema path-aware: a single-file path advertises the `content` column; a directory
   advertises the listing schema. (Or document a stable superset schema with `content` nullable.)
4. Hermetic tests: single-file read returns bytes; directory read unchanged; sandbox escape rejected.

## Key files

- `crates/driver-local/src/{read.rs,row.rs,lib.rs,fs_core.rs}`.

## Considerations

- Do NOT change the directory-listing shape (cookbook `/local` dir-list recipe relies on it).
- Large files: stream/seam — note any size cap; the codec path (T2) consumes the bytes column.
- This unblocks T2; on its own it makes `/local/<file>` content addressable for the first time.
