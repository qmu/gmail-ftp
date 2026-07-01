---
created_at: 2026-06-30T20:30:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash: c1623e2
category: Changed
depends_on: [20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md]
---

# Drive FTP parity gaps: file `get` (download content) + verify writes are live

Part of EPIC `20260630203000`.

## gdrive-ftp commands → qfs (the gaps)

- **`get` — download a file's content.** qfs wired **folder listing** (`read_rows` folder walk,
  commit `0ed4df1`) but NOT a single file's **content** download through the read facet. The pure
  pieces exist (`crates/driver-gdrive/src/read.rs::plan_read` + `decode_body` + the export path for
  Google-native docs), but they are not bridged into `DriveReadDriver`. Wire `/drive/.../<file>`
  (or `id:<fileId>`) → resolve id → download (or export native docs) → bytes/rows.
- **`put` / `mkdir` / `rm` (trash) / cp / mv — verify they COMMIT live.** The driver models them
  (`crates/driver-gdrive/src/{effect.rs,applier.rs}`; REMOVE=trash, Cp/Mv, upload/update_content),
  and the Google apply stack registers in `crate::commit`. Confirm `upsert into /drive/...` (upload),
  folder create, `remove` (trash) actually apply for a connected account, and fix any unwired leg.

## Key files

- `crates/driver-gdrive/src/{read.rs,path.rs,export.rs,client.rs,effect.rs,applier.rs}`.
- `crates/qfs/src/read_facets.rs` (`DriveReadDriver`), `crates/qfs/src/{shell.rs,commit.rs}`.

## Considerations

- Folder listing already lists children metadata; this is the **blob content** read + the write
  legs. Hermetically testable via `MockDriveClient` (`with_download`, etc.); live under EPIC
  `20260630203030`.
- Google-native docs (Docs/Sheets) have no raw bytes — `export` to a concrete MIME (the plan_read
  Export arm) is the `get` for those.

## Final Report

Development completed as planned. The headline gap — single-file content `get` — is wired into
`read_rows`: a `/drive/...` (or `id:<fileId>`) path that resolves to a **file** now downloads its
raw bytes (or exports a Google-native doc via the existing `plan_read` Export arm) into a one-row
`content` batch (name + mime_type + the `content` Bytes column the engine's `DECODE` reads), exactly
mirroring the `/local/<file>` and `/git/<repo>/<file>` content reads. A **folder** path still lists
children. The write legs (put/mkdir/rm/cp/mv) were verified WIRED — modeled in `effect.rs`, applied
in `applier.rs`, registered in the live commit path (`register_google` → `DriverId("drive")`), all
hermetically tested; the LIVE commit proof against a real account is EPIC `20260630203030`.

### Discovered Insights

- **Insight**: The file-vs-folder decision needs the node's `mime_type`, which the name-walk already
  fetches — so `resolve_node` returns the final `FileMeta` (not just an id) and the My/Shared paths
  avoid an extra `get_file` round-trip. Only an `id:<id>` address (where no walk happened) calls
  `get_file` to learn the node type.
  **Context**: This is why the `by_id` read now records one `get_file` + one `list_files`, while a
  My/Shared file read records only its walk `list_files` calls + the download — the seam reuses the
  metadata the walk already paid for.
- **Insight**: A path resolving to a file used to return an EMPTY listing (it listed the file's
  non-existent children), not an error — a silent wrong-answer. The fix makes a file path return
  content; any test seeding a file under an `id:` now needs `with_file` so `get_file` resolves it.
  **Context**: Mirrors the same class of bug as the git-blob ticket (`20260630203100`): a single-blob
  path that fell through the listing path instead of reading content.
