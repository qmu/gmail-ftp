---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: ac71c1d
category: Added
depends_on: [20260622214650-t19-driver-google-oauth-multi-account.md]
---

# Driver: Google Drive

## Overview

This ticket delivers the **Google Drive blob/namespace driver**, mounting Drive under
`/drive/...` so that an AI agent (or human) operates Drive through the same uniform,
filesystem-shaped pipe-SQL grammar as every other backend. It implements the **Blob /
namespace archetype** from RFD §5 (native verbs `ls cp mv rm`, mapped onto cfs universal
verbs `SELECT / UPSERT / REMOVE` and the `cp/mv` cross-mount operators), the
**`@version` temporal coordinate** for Drive revisions (RFD §4 — `/drive/file@<rev>`),
and the **effects-as-data + purity invariant** (RFD §3, §6): every write evaluates to a
`Plan` node and only `COMMIT` performs I/O.

Drive is special among blob backends: Google-native docs (Docs/Sheets/Slides) have **no
raw bytes** — a read must *export* to a concrete office/text format. This driver encodes
that as a capability + codec-aware read path, so `cat`/`cp` of a Google Doc yields a
deterministic exported artifact rather than an error. It also covers **Shared Drives**,
**streaming** up/download (no whole-file buffering), and folder-tree navigation over
Drive's non-hierarchical (parent-pointer, multi-parent) storage model.

This is one of the two drivers (with Gmail) that subsume the legacy Go `../gdrive-ftp`
tool (RFD preamble): it is *reimplemented*, not merged.

## Scope

In scope:
- `/drive/...` mount: folder-tree resolution, `ls` (list children), path→file-id resolution.
- Read: `SELECT`/`cat` returns blob bytes; Google-native docs **export** to office formats.
- Write: `UPSERT INTO /drive/<path>` (create-or-update by path), `REMOVE` (trash/delete).
- Cross-mount `cp` / `mv` planning (copy→verify→delete recovery semantics from RFD §6).
- **Shared Drives** (`/drive/shared/<driveName>/...`) alongside My Drive (`/drive/my/...`).
- **Streaming** download (resumable GET, range) and **resumable upload** for large files.
- `@rev` revision addressing: list revisions, read a pinned revision (`/drive/file@<rev>`).
- Capability declaration + structured parse-time rejection of unsupported ops (RFD §5).
- Owned DTOs (no `google-drive3`/serde-of-vendor types leak past the driver boundary).

Out of scope (deferred):
- OAuth token acquisition, multi-account credential store, refresh — **t19** (depends_on).
- Pushdown of `WHERE`/`SELECT` predicates into Drive's `q` search query language —
  **separate federation/pushdown ticket** (E3); this ticket does local filtering only,
  except the minimal `name`/`parent`/`trashed` filters needed for path resolution.
- Codec registry internals (`DECODE`/`ENCODE` for json/csv/md) — **E1 codec ticket**; this
  driver only *selects* an export MIME and emits bytes, it does not parse them to rows.
- Server bindings (`CREATE TRIGGER ON /drive change`) — **E7**; the change-watch poller is
  not built here (only the read/write surface it will later consume).
- Generic blob trait shared with S3/R2/FS — if not yet present, this ticket defines the
  Drive-specific impl; extraction of a shared `BlobDriver` super-trait is **E4 cleanup**.

## Key components

New crate/module `cfs-driver-gdrive` (or `crates/drivers/gdrive/`), behind a thin HTTP
client (RFD §9 — no heavy vendor SDK; `reqwest` + owned DTOs).

- `struct GDriveDriver { http, accounts }` implementing the core `Driver` trait:
  - `fn namespace(&self) -> Namespace` — declares the `/drive` path tree:
    `my/`, `shared/<driveName>/`, each folder node = blob-namespace archetype.
  - `fn capabilities(&self, node) -> Capabilities` — `SELECT|UPSERT|REMOVE|CP|MV` per node;
    `@version` supported; **`INSERT` of arbitrary columns rejected** (blob, not relational).
  - `fn schema(&self, node) -> Schema` — columns powering `DESCRIBE`: `id, name, path,
    mime_type, size, modified_time, md5, is_google_doc, rev, parents, drive_id, trashed`.
  - `fn plan_read(&self, sel) -> Plan` / `fn plan_write(&self, eff) -> Plan` — construct
    effect nodes, **no I/O** (purity invariant, RFD §3).
  - `fn procedures(&self) -> &[ProcDecl]` — none required for v1 (CRUD is universal); leave
    a slot for future `CALL drive.share(...)` / `drive.export(...)`.
- Owned DTOs (serde, `#[serde(rename_all="camelCase")]`), never exposed past the boundary:
  `FileMeta`, `Revision`, `SharedDrive`, `Permission`, `ExportTarget`, `ListPage`.
- `enum DriveEffect { Upload{..}, Update{..}, Copy{..}, Move{..}, Trash{..}, Delete{..} }`
  — leaf nodes the planner places into the typed effect DAG; each carries
  `irreversible: bool` (`Delete`/`Trash` true) for RFD §6 PREVIEW gating.
- `enum DrivePath { MyRoot, My(Vec<Segment>), SharedRoot, Shared{ drive, Vec<Segment> }, ById(FileId) }`
  + `fn resolve(&self, path) -> Result<Resolved, DriveError>` walking parent pointers,
  caching `name→id` per folder; handles **multi-parent** files and `id:<fileId>` shorthand.
- Export mapping `fn export_target(mime: &str) -> Option<ExportTarget>`:
  `application/vnd.google-apps.document → docx`, `…spreadsheet → xlsx`,
  `…presentation → pptx`, `…drawing → pdf/svg`; non-google mimes → raw passthrough.
  An explicit suffix on the path (`report.gdoc!pdf` or `?export=pdf`) overrides the default.
- Streaming: `fn open_read(file, rev) -> impl AsyncRead` (range/resumable GET, bounded
  buffer) and `fn open_write(...) -> ResumableUpload` (Drive resumable upload session,
  chunked, retry/resume on the session URI — idempotent on the same session).
- `struct GDriveClient` — thin async HTTP over Drive v3 (`files`, `files.export`,
  `revisions`, `drives`, resumable `upload`), pagination via `nextPageToken`, exponential
  backoff on `403 userRateLimitExceeded`/`429`/`5xx`. Borrows tokens from the **t19**
  account store via an injected `TokenSource` trait (no token handling here).

## Implementation steps

1. Scaffold `cfs-driver-gdrive`; add `GDriveDriver` registered into the path registry at
   mount `/drive` (RFD §3 — new service = new mount, zero new keywords).
2. Define owned DTOs + `GDriveClient` over Drive v3 with a `TokenSource` seam (t19);
   implement `files.list` (paged), `files.get`, `drives.list`.
3. Implement `DrivePath::resolve` (My Drive + Shared Drives) with per-folder name→id cache
   and multi-parent handling; wire `ls` → `SELECT` over folder children.
4. Implement capabilities + schema + `DESCRIBE` output; reject unsupported verbs at parse
   time with a structured `DriveError` (RFD §5 — important for AI).
5. Read path: raw download for binary files; `export_target` + `files.export` for Google
   docs; path-suffix/`?export=` override; streaming `open_read` with range support.
6. `@rev`: `revisions.list`/`get`; resolve `/drive/file@<rev>` to a pinned read; surface
   `rev` as a column.
7. Write path: `UPSERT` → resumable upload (create vs. update by resolved id); `REMOVE` →
   trash (default) or hard `delete` (flagged irreversible); emit `DriveEffect` plan nodes.
8. Cross-mount `cp`/`mv`: planner emits copy→verify(md5/size)→(delete for mv) per RFD §6;
   same-drive server-side copy via `files.copy` where possible, else stream relay.
9. Plan/COMMIT integration: ensure all writes go through PREVIEW (counts + irreversible
   flags) and apply only on COMMIT; record applied effects to the audit ledger.
10. Tests: golden plan assertions (no live creds) + recorded-HTTP integration tests.

## Considerations

- **Least privilege & secrets (RFD §10):** request the narrowest Drive scopes that satisfy
  read+write+export; tokens are owned by t19's encrypted store and **never logged**. The
  driver receives a short-lived token via `TokenSource` and must not persist it.
- **Idempotency / recovery (RFD §6):** `UPSERT` is the retry-safe write; resumable uploads
  resume on the same session URI so a retried COMMIT does not duplicate a file. `mv` is
  copy→verify→delete and the audit ledger is the reconstruction source on partial failure.
  Use Drive `version`/ETag for optimistic-concurrency read-then-write (`@rev` mismatch →
  structured conflict, not silent overwrite).
- **The genuinely hard parts:**
  1. *No raw bytes for Google docs* — export is lossy and format-dependent; make the chosen
     export MIME explicit in the plan and in `DESCRIBE`, and let the path override it, so the
     agent's read is deterministic and self-documenting.
  2. *Non-hierarchical, multi-parent storage* — a "path" is a convenient fiction over
     parent pointers; resolution must pick/refuse ambiguous multi-parent placements and
     cache name→id without going stale across a plan (snapshot resolution at plan time).
  3. *Shared Drives* differ (require `supportsAllDrives`, `driveId`, `corpora=drive` on every
     call, different permission model) — thread these flags through `GDriveClient` uniformly.
  4. *Rate limits & large files* — bounded retries with backoff, per-leg timeouts, circuit
     breaker (RFD §6 observability); stream, never buffer whole files.
- **No vendor leak (RFD §9):** `reqwest`/serde DTOs stay inside the driver; the engine sees
  only cfs `Row`/`Plan`/`DriveError`. Keep the driver a *consumer-side small trait* impl.
- **Directory/coding standards:** driver lives under `crates/drivers/gdrive/`; effects are
  data (`enum`), purity invariant holds (functions return `Plan`); `clippy -D warnings`.

## Acceptance criteria

- `cargo build` and `cargo clippy -- -D warnings` are green for the new crate.
- `DESCRIBE /drive` and `DESCRIBE /drive/my/<folder>` emit the declared schema/capabilities;
  an unsupported op (e.g. relational `INSERT` of columns) is rejected **at parse time** with
  a structured error (asserted in tests).
- **Plan assertions (no live creds):** `UPSERT INTO /drive/my/a/b.txt` produces a single
  `Upload`/`Update` effect with correct resolved parent id; `REMOVE` produces a `Trash`
  (or `Delete` with `irreversible=true`) effect; `cp /drive/... /s3/...` produces a
  copy→verify(+delete for `mv`) DAG. Verified via golden snapshots; PREVIEW performs no I/O.
- **Read/export:** reading a `application/vnd.google-apps.document` plans an `export` to
  `docx` by default and honors a path/`?export=` override; reading a binary file plans a
  raw streamed download. Asserted against recorded HTTP fixtures.
- `/drive/file@<rev>` resolves to a specific revision and is reflected in the `rev` column.
- Shared Drive paths (`/drive/shared/<name>/...`) resolve and list correctly against a
  recorded `drives.list`/`files.list` fixture (`supportsAllDrives` flags present).
- Integration tests use recorded fixtures only — **no live network, no real credentials** —
  and pass in CI.
