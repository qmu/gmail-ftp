---
created_at: 2026-06-30T20:30:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash:
category: Added
depends_on: [20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md]
---

# Gmail FTP parity gaps: `ls /` (labels) + message `get` (download)

Part of EPIC `20260630203000`. Close the gmail-ftp gaps qfs doesn't yet do.

## gmail-ftp commands → qfs (the gaps)

- **`ls /` — list labels.** gmail-ftp's root lists labels (INBOX, SENT, …, user labels). qfs's read
  facet handles `/mail/<label>` and `/mail/drafts` but NOT a `/mail` root that lists labels. Wire a
  label listing (the Gmail `labels.list` API; the mock client + `MockGmailClient::with_labels` seam
  exists). Decide the path: `/mail` (root) or `/mail` describe → label rows.
- **`get` — download a message / attachment.** gmail-ftp `get` exports a message to `.eml` and lists
  `<message>/` attachments. qfs reads message rows; verify/wire reading a single message's full
  content (and its attachments as nested entries — the driver doc says "attachments = nested
  entries"). May need an `id:` message-content read path + attachment listing.

## Already works (no action)

`/mail/<label>` read (WHERE→q= + LIMIT pushdown, commit `e14862d`), draft insert, `call mail.send`
(irreversible), trash (`remove`), label add/remove columns — all in the driver + commit registry.

## Key files

- `crates/driver-gmail/src/{read.rs,path.rs,schema.rs,client.rs}` (read_rows, MailPath, labels seam).
- `crates/qfs/src/read_facets.rs` (`GmailReadDriver`), `crates/qfs/src/shell.rs` (registration).

## Considerations

- Hermetically testable via `MockGmailClient` (no live account) — add mock tests for label listing +
  message-content read; live-verify under EPIC `20260630203030`.
- Map each verb to the gmail-ftp command in the EPIC's guidance doc (`20260630203040`).
