---
created_at: 2026-06-30T20:30:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash: ba12a18
category: Changed
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

## Final Report

Development completed as planned, with a **scoped boundary** on attachments. Both stated gaps were
wired in `read_rows`: `/mail` (root) → label listing (`name` rows, gmail-ftp `ls /`); a single
message node (`/mail/<label>/<msg>` or `id:<msg>`) → that message's row, headers + snippet +
attachments-as-nested-entries (gmail-ftp `get`). `DESCRIBE /mail` now reports the label schema (not
the message schema) so introspection stays honest.

**Boundary (NOT done, deliberately):** attachment *bytes* fetch. The crate's own module docs already
park this — the `GmailClient` trait has no `get_attachment` method, so an attachment node
(`/mail/<label>/<msg>/<att>`) still fails closed with `invalid_path`. Adding the bytes fetch needs a
new client method + the real `messages.attachments.get` API shape, which can only be live-verified —
so it belongs with EPIC `20260630203030` (live verification) or its own follow-up. The message read
already surfaces the attachment *manifest* (filename/mime/size) as nested struct entries, which is
the structured-data win.

### Discovered Insights

- **Insight**: `caps_for` already advertised `Root → Ls/Select` and `Message → Select`, but
  `read_rows` returned `invalid_path` for both — the capability surface promised reads the read path
  didn't fulfill. Wiring them made the advertised capability true (the "make docs true" bar): the
  capability table is the contract, and a gap between it and `read_rows` is a silent lie.
  **Context**: When adding a Gmail read node, update BOTH `caps_for` (the gate) and `read_rows` (the
  fulfillment) — and `describe` if the node's row shape differs from `MailMessage` (the root does).
