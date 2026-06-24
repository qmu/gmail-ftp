---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash: 1309791
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t15-codec-registry-decode-encode.md]
---

# Driver: local filesystem

## Overview

This ticket delivers the **first concrete driver**: a blob/namespace driver over the
host filesystem, mounted at `/local/...`. It is the reference implementation of the
`Driver` trait (#13) and the simplest member of the **Blob / namespace archetype**
(RFD §5 table: native verbs `ls cp mv rm`). Because it needs no network, no
credentials, and no vendor SDK, it is the proving ground for the whole driver contract
and the runtime's effect-plan path (RFD §2.3, §6).

Crucially, it is the **anchor that makes `cp` local↔cloud native** (RFD §1, §7: "cp
spans mounts without leaving cwd"). Every cloud driver's copy/upload/download story is
expressed as a cross-source plan with `/local/...` on one side; getting streaming
reads/writes and the `cp = copy→verify→delete` recovery shape (RFD §6) right here
defines the pattern for S3/Drive/R2. Paired with the codec registry (#15), a local
`.md`/`.json`/`.csv` blob becomes a queryable/editable relation via `DECODE`/`ENCODE`
(RFD §4) — e.g. `.workaholic/**/*.md` as a table — with **zero** new keywords.

## Scope

In scope:
- A `LocalFsDriver` implementing the `Driver` trait (#13): namespace declaration,
  per-node archetype/schema (`DESCRIBE`), capabilities, pushdown declaration.
- Path mount `/local/<abs-or-rooted path>` with a configurable sandbox root.
- Native blob ops realized as universal verbs over plan effects:
  `ls` (= `FROM /local/dir` scan → rows of {name, path, size, modified, is_dir, mode}),
  `cat` (streaming blob read), `cp`/`mv` (effect nodes), `rm`/`REMOVE`, and
  `UPSERT INTO /local/path` (write/overwrite a blob).
- **Glob** resolution including recursive `**` for `FROM`/`cp`/`rm` path sets.
- **Streaming** reads and writes (bounded memory; no whole-file buffering).
- Apply-side execution of the local effect nodes (the impure leg the runtime calls),
  with `cp = copy→verify(size+hash)→[delete for mv]` and a temp-file + atomic rename
  for `UPSERT`/write.

Out of scope (deferred):
- The `Driver`/`Codec` trait definitions themselves → **#13 / #15**.
- Effect-plan types, `PREVIEW`/`COMMIT`, batching/parallelism, the audit ledger →
  **#9 / #10**; transactions/idempotency/optimistic concurrency → **#11**.
- Cloud drivers (S3/R2/Drive) and the *other* end of cross-mount `cp` → sibling E4
  driver tickets.
- `@version` temporal addressing (FS has no native versioning) → not applicable here.
- Watch/inotify-based `TRIGGER` event sourcing → E7 server.

## Key components

New crate/module `cfs-driver-local` (infrastructure), depending on `cfs-driver`
(the `Driver` trait + DTOs, #13) and `cfs-codec` (#15) only at the registry boundary:

- `struct LocalFsDriver { root: PathBuf, read_only: bool }` — the mount; `root` is the
  least-privilege sandbox boundary.
- `impl Driver for LocalFsDriver` — `fn declare(&self) -> DriverDecl` (namespace tree,
  archetype = `Blob`, schema columns, capabilities `{ls,cp,mv,rm,upsert,remove}`,
  pushdown = `{Scan, Glob, ProjectName}`, empty procedure set, no prelude).
- `fn scan(&self, path: &VfsPath, pred: &Pushdown) -> Result<RowStream, DriverError>`
  — directory/glob listing → owned `Row` DTOs (no `std::fs::DirEntry` leak past the
  boundary, RFD §9).
- `fn open_read(&self, path) -> Result<Box<dyn AsyncRead + Send>, DriverError>` and
  `fn open_write(&self, path, mode) -> Result<BlobWriter, DriverError>` — streaming
  blob I/O; `BlobWriter` writes to a sibling temp file then atomic-renames on `finish`.
- `fn apply(&self, effect: &LocalEffect) -> Result<EffectOutcome, DriverError>` — the
  one impure entry the runtime invokes per local effect node.
- `enum LocalEffect { Write{dst, src}, Copy{src, dst}, Move{src, dst}, Remove{path} }`
  — owned effect DTOs the evaluator emitted; `Move`/`Remove` flagged `irreversible`.
- `fn resolve_glob(&self, pat: &str) -> Result<Vec<PathBuf>, DriverError>` — `**`/`*`/`?`
  expansion, scoped to `root`, symlink-cycle safe.
- `struct LocalRow { name, path, size, modified, is_dir, mode }` — the schema DTO.
- `enum DriverError { OutsideSandbox(String), NotFound(String), AlreadyExists(String),
  Io(String), CapabilityDenied{path,verb} }` — structured (RFD §5, AI-consumable).

## Implementation steps

1. Scaffold `cfs-driver-local`; implement `path_resolve(&root, vfs_path)` that
   canonicalizes and **rejects any path escaping `root`** (`..`, symlink) → `OutsideSandbox`.
2. Implement `declare()` returning the `DriverDecl`: blob archetype, `LocalRow` schema,
   capability set, pushdown set, no procs/prelude.
3. Implement `scan` for a single directory → owned `LocalRow` stream (lstat metadata).
4. Implement `resolve_glob` (`*`, `?`, `**`) over `root`; wire it into `scan` so
   `FROM /local/**/*.md` yields the recursive set; bound symlink traversal.
5. Implement streaming `open_read` (`cat`) and `open_write` (temp file + atomic rename).
6. Implement `apply` for each `LocalEffect`: `Write` (UPSERT), `Copy`
   (stream copy→verify size+hash), `Move` (copy→verify→unlink), `Remove`.
7. Honor `read_only`: reject write/effect ops with `CapabilityDenied` before any I/O.
8. Register the driver + its codec touchpoints so `DECODE/ENCODE` (#15) compose over
   local blobs (no driver-specific codec code — pure `bytes↔rows`).
9. Tests: unit (sandbox escape, glob, atomic write, streaming roundtrip) + golden
   plan/scan snapshots over a `tempdir` fixture; an integration test exercising a
   local→local `cp`/`mv` plan via the runtime.

## Considerations

- **Least privilege / sandbox (RFD §10):** every path crosses `path_resolve` against
  `root` first; symlinks are canonicalized and re-checked so a link cannot point
  outside the mount. No secrets handled, but `root` confinement is the blast-radius
  control. Never log full file contents; log path + byte counts only.
- **Idempotency & recovery (RFD §6):** `UPSERT`/write is retry-safe via temp-file +
  atomic `rename` (no torn writes; re-running overwrites cleanly). `cp` follows
  copy→verify→[delete]: on `mv`, the source is unlinked **only after** the destination
  is byte/hash-verified, so a crash leaves the source intact — the recoverable shape
  cloud drivers reuse.
- **Streaming (hard part):** reads and writes must be bounded-memory streams, not
  `read_to_end`; large-file `cp` streams through a fixed buffer and computes the verify
  hash incrementally. This is the contract the runtime's batched/parallel apply (#10)
  relies on.
- **Glob semantics (hard part):** `**` recursion, ordering determinism (sort entries
  for stable golden snapshots), and symlink-cycle protection. Decide and document
  whether dotfiles match `*` (default: no, like shells) — make it explicit.
- **Owned DTOs / no vendor leak (RFD §9):** only `LocalRow`/`LocalEffect`/streams cross
  the `Driver` boundary; `std::fs` types stay internal.
- **Observability (RFD §6):** `apply` returns an `EffectOutcome` (bytes moved, dst path,
  verify status) for the audit ledger; per-effect errors are structured, never panics.
- **Concurrency:** atomic rename gives last-writer-wins; true read-then-write optimistic
  concurrency (mtime/ETag) is deferred to #11 — note the seam (`open_write` can later
  accept an expected-mtime precondition).
- **Directory/standards:** infrastructure crate, `#![forbid(unsafe_code)]`, `thiserror`
  for `DriverError`, async I/O via the workspace runtime; follow workspace clippy config
  and doc every public item.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green;
  `cargo test -p cfs-driver-local` passes.
- `declare()` reports the blob archetype, `LocalRow` schema, and a capability set that
  rejects unsupported verbs at parse/eval time (asserted via a denied-verb test).
- `FROM /local/**/*.md` over a `tempdir` fixture yields the correct recursive row set in
  a deterministic order (golden snapshot); a path containing `..`/escaping symlink
  returns `OutsideSandbox` and performs no I/O.
- Streaming `cat` of a multi-MB fixture roundtrips byte-identically with bounded memory
  (no `read_to_end` in the read path — verified by review/bench).
- `UPSERT INTO /local/p` is atomic: a write interrupted before `finish` leaves the
  original intact (temp file is discarded) — asserted by a fault-injection test.
- A local→local `cp` plan copies then verifies (size+hash); the equivalent `mv` deletes
  the source only after verification (plan + post-apply state assertions).
- A `read_only` mount rejects every write/effect with `CapabilityDenied` and touches no
  files.
- No network access and no live credentials anywhere in the test suite.
