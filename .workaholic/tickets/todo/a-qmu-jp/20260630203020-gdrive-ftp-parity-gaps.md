---
created_at: 2026-06-30T20:30:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash:
category: Added
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
