---
created_at: 2026-06-30T01:00:30+09:00
author: a@qmu.jp
type: bugfix
layer: [Domain]
effort: 2h
commit_hash: 4d870c2
category: Changed
depends_on: []
---

# Local writes: accept positional / non-`content` write payloads (and correct the stale roadmap text)

The roadmap claims *"`upsert into /local/<file> …` previews and says it committed, but doesn't yet
write the file to disk."* **This is stale — local writes DO persist to disk.** Discovery ran the
release binary:

- `upsert into /local/<abs>/wrote.txt values (content) ('hello') --commit` → `"committed":true` and
  the file was actually written (verified on disk). The apply path is fully wired:
  `crates/qfs/src/main.rs:77` injects the real `commit::apply_plan`; `commit.rs:235 live_registry`
  registers `local` via `qfs_driver_local::local_apply_driver`; `crates/driver-local/src/applier.rs:52`
  does real I/O (`fs_core::write_blob_atomic`), proven by `crates/driver-local/tests/e2e_commit.rs`.
  (The "says committed, no I/O" in-memory path only triggers when **no** WorldApply is injected —
  `crates/exec/src/exec.rs:179` — which is not the case for `qfs run`.)

## The real, narrower gap

A write payload that doesn't carry a column literally named `content` fails at commit instead of
writing:

- `upsert into /local/.../wrote.txt values ('hello')` (positional, no column name) → hard error:
  `commit_failed: Terminal { reason: "write … carries no \`content\` blob payload and no \`src\`
  source" }`.
- `crates/driver-local/src/effect.rs:118 decode_write` requires the first row to carry `CONTENT_COL`
  (`content`). The evaluator/write-planner doesn't map a generic write payload (positional `VALUES`,
  or a piped relation whose blob column has another name) onto `CONTENT_COL`.

## Plan

1. Map a single-column / positional write payload onto the `content` blob in
   `crates/driver-local/src/effect.rs:103 decode_write` (or in the write-planner that builds the
   effect), so `values ('hello')` and a piped relation with one blob column both reach the working
   `write_blob_atomic`. Keep the explicit `content`/`src` forms working.
2. **Correct the roadmap**: rewrite `docs/roadmap.md` "Actually write local files" bullet (~line 130)
   — local writes work; the remaining gap is non-`content` payload mapping. Don't leave a false gap.
3. Tests: positional `VALUES`, and a `… |> select <blob> |> upsert into /local/...` pipe.

## Key files

- `crates/driver-local/src/effect.rs:103` (`decode_write`, `CONTENT_COL`), the write-planner that
  builds `LocalEffect::Write`, `docs/roadmap.md` (stale bullet).

## Considerations

- Bump the patch in `crates/qfs/Cargo.toml`. `docs/roadmap.md` is hand-authored (safe to edit).
