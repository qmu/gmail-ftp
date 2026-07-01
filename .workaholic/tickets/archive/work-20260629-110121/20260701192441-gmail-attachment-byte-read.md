---
created_at: 2026-07-01T19:24:41+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: 1ffb11c
category: Added
depends_on:
---

# Gmail attachment byte-read path (get_attachment)

## Overview

**Sub-ticket of EPIC `.workaholic/tickets/todo/a-qmu-jp/20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`.** gmail-ftp can download an attachment's bytes (`get id:att:<msg>:<att>`); qfs cannot. Message rows expose attachment **metadata** only (`filename`/`mime`/`attachment_id`/`size`), and reading an attachment node `/mail/<label>/<msg>/<att>` currently **fails closed** with `invalid_path`.

Discovery confirms this is an explicit, self-contained park (`driver-gmail/src/lib.rs` doc comment lines ~40-46): the path variant already **parses** (`MailPath::Attachment`) and already has a `Select` capability (`lib.rs` ~204), but there is **no** `GmailClient::get_attachment` method and **no** `read_rows` arm behind it. This ticket wires that one leg. It is **independent** of the draft-attachment work (no dependency on `20260701192439`).

Experimental posture: additive; patch bump on the shipped PR.

## Policies

- `workaholic:implementation` / `policies/directory-structure.md` — applies to all code work.
- `workaholic:implementation` / `policies/coding-standards.md` — applies to all code work.
- `workaholic:implementation` / `policies/domain-layer-separation.md` — `get_attachment` lives in `client.rs` and returns an owned vendor-free `Attachment` DTO; Gmail JSON / base64url decoding never crosses the client seam.
- `workaholic:implementation` / `policies/type-driven-design.md` — decode into the existing typed `Attachment { filename, mime, bytes }`, not raw strings; return `Result`.
- `workaholic:implementation` / `policies/test.md` — extend `MockGmailClient` + `RecordedCall`; the flipped `read.rs` test proves the wiring hermetically.
- **Anti-drift (CLAUDE.md):** update `docs/cookbook/gmail.md` attachment-download recipe and regenerate SKILL.md; recipe parse-checked by `cookbook_skills.rs`.

## Key Files

- `packages/qfs/crates/driver-gmail/src/client.rs` - `GmailClient` trait + real/mock impls; add `get_attachment(msg_id, att_id) -> Result<Attachment, GmailError>` (real: `GET messages/{id}/attachments/{attId}`, base64url-decode `data`).
- `packages/qfs/crates/driver-gmail/src/read.rs` - `read_rows`; add the `MailPath::Attachment` arm emitting a `content`/bytes row. Flip the `an_attachment_node_has_no_backing_read_yet` test (~186-193).
- `packages/qfs/crates/driver-gmail/src/path.rs` - `MailPath::Attachment` (already parses; no change expected).
- `packages/qfs/crates/driver-gmail/src/lib.rs` - the `Select` cap on the attachment node (~204) and the park note (~40-46) to update.
- `packages/qfs/crates/driver-gmail/src/schema.rs` - the `Attachment { filename, mime, bytes }` DTO the read decodes into.
- `docs/cookbook/gmail.md` / `docs/guide/replace-gmail-gdrive-ftp.md` - attachment-download recipe / migration row (line ~87).

## Related History

- [20260630203010-gmail-ftp-parity-gaps.md](.workaholic/tickets/archive/work-20260629-110121/20260630203010-gmail-ftp-parity-gaps.md) - Its Final Report explicitly parked this exact boundary: "the GmailClient trait has no get_attachment method, so an attachment node still fails closed with invalid_path." Reuse its `caps_for` + `read_rows` pattern and mock seam.

## Implementation Steps

1. Add `get_attachment` to the `GmailClient` trait; implement in the real client (fetch, base64url-decode into `Attachment`) and in `MockGmailClient` (record + return a fixture), extending `RecordedCall`.
2. Add the `MailPath::Attachment` arm in `read.rs::read_rows` to emit a row with `filename`/`mime`/`size`/`content(Bytes)`.
3. Flip `an_attachment_node_has_no_backing_read_yet` to assert a real row; add boundary tests (missing attachment id, empty attachment).
4. Update the `lib.rs` park note.
5. Add the cookbook download recipe + migration-guide row; run `gen-docs` / `gen-skills`.

## Quality Gate

**Acceptance criteria:**

- `SELECT` on `/mail/<label>/<msg>/<att>` (or `id:att:...`) returns a row with the attachment's `filename`, `mime`, `size`, and `content` bytes (previously `invalid_path`).
- The bytes decode correctly (base64url) into the typed `Attachment`; a missing/invalid attachment id errors cleanly, not panics.
- The `lib.rs` park note no longer claims the path fails closed.
- New cookbook recipe passes `cookbook_skills.rs`; patch version bumped.

**Verification method** (from `packages/qfs`, `TMPDIR` redirected, `command rm`):

- `cargo build/test/clippy/fmt` green, including the flipped `read.rs` test and new boundary tests against `MockGmailClient`.
- `cargo run -p xtask -- gen-docs --check` and `gen-skills --check` green.
- **Live proof:** against the real Google account, download an actual attachment's bytes and confirm the file opens / matches the source.

**Gate** — hermetic suite + both `--check` gates green AND a live attachment byte-download verified in-session.

## Considerations

- Independent of the draft-attachment tickets — can be driven in parallel / first.
- Keep base64url decoding inside `client.rs` (no vendor/serde types leak upward — `domain-layer-separation`).
- Large attachments: return bytes via `Value::Bytes`; note any practical ceiling.
- gmail-ftp also exports whole messages/threads as `.eml`/`.mbox` (a format feature) — that is out of scope here; qfs returns structured rows (record as a known non-goal in the migration guide).
