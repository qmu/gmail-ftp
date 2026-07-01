---
created_at: 2026-06-30T01:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash: d49e892
category: Added
depends_on: []
---

# Wire `/drive` reads for real (folder path → Drive folder-id walk)

Roadmap "Near-term backlog": connecting a Google account already works, but reading `/drive`
returns the honest *"connect a Google account"* error **even when connected** — the read facet was
deferred in the prior wire-the-binary cycle
(`archive/work-20260629-110121/20260629140100-wire-gmail-gdrive-ga-read-rows.md`). Gmail reads were
wired; Drive + Analytics were parked. This finishes Drive.

## What's missing (confirmed)

- `crates/qfs/src/shell.rs:334-336` states it outright: *"Drive (/drive) … reads need path→id
  resolution … and remain the honest connect-account facet."* `/drive` is registered only to the
  `ConnectAccountReadDriver` stub at `shell.rs:322`; **no live Drive read facet is layered over it**
  (Gmail's live facet is at `shell.rs:337-348` — mirror that).
- No `DriveReadDriver` adapter exists in `crates/qfs/src/read_facets.rs` (Gmail's is at lines
  204-227). No top-level `read_rows` exists in `crates/driver-gdrive/src/read.rs` — only `plan_read`
  (one file → ReadPlan) and `decode_body`.
- The real work — translate a folder *path* to Drive's internal folder IDs — is a named park:
  `crates/driver-gdrive/src/lib.rs:53-55` ("the snapshot folder-tree walk … is exercised through
  the mocked `list_files` seam, not a live Drive").

## Plan

1. Add `read_rows(client, path, predicate)` to `crates/driver-gdrive/src/read.rs`: parse the path via
   `DrivePath` (`crates/driver-gdrive/src/path.rs` — `My`/`Shared`/`ById`), walk each segment to a
   folder id with repeated `name = '<seg>' and '<parentId>' in parents` lookups
   (`build_query` in `query.rs` + `GDriveClient::list_files` in `client.rs`), then list children.
2. Add a `DriveReadDriver` adapter in `read_facets.rs` (twin of `GmailReadDriver`).
3. Register it over the `/drive` connect-account fallback in `shell.rs:315-349` (mirror the Gmail
   block at 337-348).

## Key files

- `crates/qfs/src/shell.rs:315-349` (registration), `crates/qfs/src/read_facets.rs` (adapter).
- `crates/driver-gdrive/src/{read.rs,path.rs,query.rs,client.rs}`, `crates/driver-gdrive/src/lib.rs:53`.

## Considerations

- The mock `list_files` seam already exists — this is wiring + a live name→id walk, not new infra.
- Honesty rule: until this lands, `/drive` keeps returning the connect-account error (correct).
- Bump the patch in `crates/qfs/Cargo.toml`; add a Drive read recipe to the cookbook ratchet if one
  fits. Sibling of the GA-read ticket (`20260630010010`) — same deferred-facet pattern.
