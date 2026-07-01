---
created_at: 2026-07-01T19:24:42+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: 6c97ae7
category: Added
depends_on:
---

# mkdir parity: create a new Gmail label + create a new Drive folder

## Overview

**Sub-ticket of EPIC `.workaholic/tickets/todo/a-qmu-jp/20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`.** Both FTP tools have a `mkdir` analogue that qfs lacks:

- **Gmail label create** — gmail-ftp `mkdir Work/Receipts`. qfs `update ... set add_labels` only applies **existing** labels; `client.rs::modify_labels` adds/removes existing label ids and there is **no** `create_label`, no path/proc/capability for it.
- **Drive folder create** — gdrive-ftp `mkdir`. Discovery found this **almost works**: `driver-gdrive/src/effect.rs::decode_upload` already turns an INSERT with `mime == FOLDER_MIME` into an `Upload{...}`, and `lib.rs` grants `Insert` on a folder path — but `client.rs::upload` (~287-297) **always** sends a media part, whereas a folder is a **metadata-only** `files.create`.

This ticket adds the Gmail label-create surface and refines the Drive folder create. Independent of the attachment tickets.

Experimental posture: additive, hard-break-friendly; patch bump on the shipped PR.

## Policies

- `workaholic:implementation` / `policies/directory-structure.md` — applies to all code work.
- `workaholic:implementation` / `policies/coding-standards.md` — applies to all code work.
- `workaholic:implementation` / `policies/domain-layer-separation.md` — new `client.create_label` and the metadata-only folder create return owned DTOs; vendor JSON stays inside `client.rs`.
- `workaholic:planning` / `policies/terminology.md` — reuse existing vocabulary (`label`, `folder`, `My Drive`/`Shared Drive`); don't coin synonyms. Decide the label-create surface consistently with existing effect-column naming (`ADD_LABELS_COL` etc.).
- `workaholic:planning` / `policies/ai-native-future.md` — new surface must fit the one grammar + describe→preview→commit loop; both creates are reversible writes (preview then `--commit`), not irreversible gates.
- **Anti-drift (CLAUDE.md):** update the migration guide's two "Not yet" rows and any cookbook mention; regenerate SKILL.md; recipes parse-checked.

## Key Files

- `packages/qfs/crates/driver-gmail/src/client.rs` - add `create_label(name) -> Result<Label, GmailError>` (real: `POST users/me/labels`; mock records it).
- `packages/qfs/crates/driver-gmail/src/lib.rs` - add the label-create capability + procedure/path (e.g. `INSERT INTO /mail/labels` or a `mail.mklabel` proc — choose per terminology/`describe` consistency).
- `packages/qfs/crates/driver-gmail/src/effect.rs` - decode the label-create effect (mirror the well-known-column pattern).
- `packages/qfs/crates/driver-gmail/src/applier.rs` - dispatch the new effect to `create_label`.
- `packages/qfs/crates/driver-gdrive/src/client.rs` - `upload` (~287-297); branch to a **metadata-only** `files.create` when `mime == FOLDER_MIME` (no media part).
- `packages/qfs/crates/driver-gdrive/src/effect.rs` - `decode_upload`; confirm a folder INSERT (empty bytes, folder mime) decodes without requiring a `bytes` cell.
- `docs/guide/replace-gmail-gdrive-ftp.md` - update the label-create "Not yet" note (line ~98) and the folder mkdir row (line ~117).

## Related History

- [20260630203020-gdrive-ftp-parity-gaps.md](.workaholic/tickets/archive/work-20260629-110121/20260630203020-gdrive-ftp-parity-gaps.md) - Verified Drive `mkdir`/`put`/`rm`/`cp`/`mv` are wired in effect/applier; this ticket completes the folder-create client leg + surfaces both mkdirs.
- [20260630203010-gmail-ftp-parity-gaps.md](.workaholic/tickets/archive/work-20260629-110121/20260630203010-gmail-ftp-parity-gaps.md) - The `caps_for` + `read_rows` + mock-seam pattern the label-create surface follows.

## Implementation Steps

1. **Gmail label create:** add `create_label` to the client trait + real + mock; add a capability/path or `mail.mklabel` proc; decode in `effect.rs`, dispatch in `applier.rs`. Reject creating a label that already exists (or make it idempotent — decide and document).
2. **Drive folder create:** in `client.rs::upload`, when `mime == FOLDER_MIME`, issue a metadata-only `files.create` (no media part); confirm `decode_upload` accepts a folder INSERT with no `bytes`.
3. Add hermetic tests: mock label-create records the `POST labels` call and returns the new label; mock folder-create records a metadata-only `files.create` (no media part).
4. Update the migration guide's two "Not yet" rows; run `gen-docs` / `gen-skills`.

## Quality Gate

**Acceptance criteria:**

- Creating a **new** Gmail label via the chosen qfs surface previews, then on `--commit` calls `POST users/me/labels` and the label appears (asserted via mock; verified live).
- Creating a **new** Drive folder via `INSERT` with `mime == FOLDER_MIME` issues a **metadata-only** `files.create` (no media part) and the folder appears.
- Both are reversible writes (preview by default; `--commit` to apply) — neither trips the irreversible gate.
- Migration guide's two "Not yet" rows updated; recipes parse-check; patch version bumped.

**Verification method** (from `packages/qfs`, `TMPDIR` redirected, `command rm`):

- `cargo build/test/clippy/fmt` green, incl. the new mock assertions (label-create call; folder metadata-only create).
- `cargo run -p xtask -- gen-docs --check` and `gen-skills --check` green.
- **Live proof:** against the real Google account, create a new label and a new Drive folder; confirm both exist in Gmail / Drive.

**Gate** — hermetic suite + both `--check` gates green AND both creates verified live in-session.

## Considerations

- Decide the Gmail label-create surface deliberately (`INSERT INTO /mail/labels` vs a `mail.mklabel` proc) so `describe` stays honest and terminology is consistent (`terminology`).
- Define behavior for an already-existing label/folder name (idempotent vs error) and document it.
- Independent of the attachment tickets; can be driven in parallel.
- Drive Google-native **export** on write (create/edit a Docs file) remains a non-goal — read-side export already exists; note it in the guide.
