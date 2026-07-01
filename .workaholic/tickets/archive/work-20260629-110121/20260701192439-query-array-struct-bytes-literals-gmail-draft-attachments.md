---
created_at: 2026-07-01T19:24:39+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: 9cbd7df
category: Added
depends_on:
---

# Array/Struct/Bytes literals in the query language + Gmail draft attachments end-to-end

## Overview

**Sub-ticket of EPIC `.workaholic/tickets/todo/a-qmu-jp/20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`** (replace gmail-ftp/gdrive-ftp with qfs). This is the **foundation** ticket: the other draft-attachment work depends on it.

Discovery found the Gmail driver already accepts a draft `attachments` column end-to-end — `effect.rs::attachments_col` reads an `Array(Struct{filename, mime, bytes})` column and `mime.rs` builds the multipart/mixed message from it — but **no query can produce that column**. The parser's `Literal` enum (`ast.rs`) has only `Str/Int/Float/Bool/Null/Size/Typed`, and `core/src/eval.rs::literal_to_value` maps only those scalars; there is no `Array`, `Struct`, or `Bytes` literal. So `INSERT INTO /mail/drafts` can only ever supply `(to, subject, body)`.

This ticket adds **general `Array`/`Struct`/`Bytes` literal constructors to the closed-core grammar** (the owner's chosen approach — reusable across any nested column, not a narrow `attach()` builtin), and proves the payoff by attaching a **local** file to a draft and sending it. (Piping a *Drive* download into the attachment is the follow-on ticket `20260701192440`.)

Per project policy (memory): qfs is experimental — no backward-compat/migration shims; this is a straight additive change. Per README SemVer it is a MINOR-class registry/grammar-adjacent change, but the operational rule still **bumps the patch** in `packages/qfs/crates/qfs/Cargo.toml` on the shipped PR.

## Policies

- `workaholic:implementation` / `policies/directory-structure.md` — conventional project layout (applies to all code work).
- `workaholic:implementation` / `policies/coding-standards.md` — Rust/style conventions (applies to all code work).
- `workaholic:implementation` / `policies/type-driven-design.md` — the new literals must lower to rich typed `Value::Array`/`Value::Struct`/`Value::Bytes`, not stringly-typed carriers; failures return `Result`, not panics.
- `workaholic:implementation` / `policies/domain-layer-separation.md` — the grammar addition lives in the closed core (parser/core), which must not depend on any driver; drivers stay downstream mapping onto the frozen `EffectKind`.
- **Anti-drift (CLAUDE.md):** generated `docs/{language,drivers,server}.md` and `plugins/qfs/skills/*/SKILL.md` are never hand-edited — change the source and run `cargo run -p xtask -- gen-docs` / `gen-skills`; every cookbook recipe is parse-checked by `crates/test/tests/cookbook_skills.rs`, so any new syntax shown in docs must actually parse.

## Key Files

- `packages/qfs/crates/parser/src/ast.rs` - `Literal` enum (lines ~579-604); add `Array`/`Struct`/`Bytes` variants.
- `packages/qfs/crates/parser/src/grammar.rs` - `literal()` (~539-551) and `values()` (~635-648); parse the new constructors (no new statement keyword — RFD "functions/values are values").
- `packages/qfs/crates/core/src/eval.rs` - `literal_to_value` (~1097-1107), `literal_value`, `values_row_batch` (~777-818); lower the new literals to `Value::Array`/`Value::Struct`/`Value::Bytes`.
- `packages/qfs/crates/core/src/typeck.rs` - literal typing (~375-380); type the new constructors (`Array(elemTy)`, `Struct(schema)`, `Bytes`).
- `packages/qfs/crates/driver-gmail/src/effect.rs` - `attachments_col` (~289-329); the already-present consumer — confirm the column contract matches (accepts `Value::Bytes` or utf8 `Value::Text` for `bytes`).
- `packages/qfs/crates/driver-gmail/src/schema.rs` - `MailDraft.attachments`, `Attachment`, `attachment_schema()` — the target schema `values_row_batch` names columns from.
- `packages/qfs/crates/driver-gmail/src/mime.rs` - multipart builder (already complete; used to assert the sent bytes).
- `docs/cookbook/gmail.md` - add the draft-with-attachment recipe (regenerates `qfs-gmail` SKILL.md).
- `docs/guide/replace-gmail-gdrive-ftp.md` - update the `put`/`compose` row (line ~89) to show the attachments column.

## Related History

The archived Gmail parity ticket wired label listing and message read but explicitly parked the write-leg proofs and the attachment surface; this ticket builds the query-language piece that unblocks draft attachments.

- [20260630203010-gmail-ftp-parity-gaps.md](.workaholic/tickets/archive/work-20260629-110121/20260630203010-gmail-ftp-parity-gaps.md) - Preceding Gmail parity work; introduced the `caps_for` + `read_rows` pattern and the `MockGmailClient` seam.
- [20260630203040-gmail-gdrive-to-qfs-guidance-doc.md](.workaholic/tickets/archive/work-20260629-110121/20260630203040-gmail-gdrive-to-qfs-guidance-doc.md) - The migration guide whose draft row this ticket updates.

## Implementation Steps

1. Add `Array(Vec<Literal>)`, `Struct(Vec<(String, Literal)>)`, and `Bytes(Vec<u8>)` (choose a byte-literal surface syntax, e.g. base64 tagged literal) to `ast.rs::Literal`.
2. Parse them in `grammar.rs::literal()` and allow them as `VALUES` cells; keep the reserved-word set frozen (no new keyword).
3. Lower in `core/src/eval.rs`: extend `literal_to_value` and `literal_value` to yield `Value::Array`/`Value::Struct`/`Value::Bytes`; ensure `values_row_batch` maps a nested cell to the target's described column type.
4. Type-check in `core/src/typeck.rs`: literal typing yields `Array(elem)`, `Struct(schema)`, `Bytes`; error clearly on element-type mismatch.
5. Confirm `driver-gmail/effect.rs::attachments_col` consumes the produced column unchanged; add a unit test that an `INSERT INTO /mail/drafts` carrying an `attachments` array round-trips into a `MailDraft` with bytes and that `mime.rs` emits the expected multipart part (assert via `MockGmailClient`).
6. Add the cookbook recipe to `docs/cookbook/gmail.md` and update the migration-guide draft row; run `gen-docs` / `gen-skills`.

## Quality Gate

**Acceptance criteria** — the checkable conditions that must hold:

- An `INSERT INTO /mail/drafts` statement that supplies an `attachments` `Array(Struct{filename, mime, bytes})` cell **parses** and **type-checks** (previously rejected as `NonLiteralValues` / parse error).
- The lowered draft reaches the driver with a populated `MailDraft.attachments` (bytes intact), and `mime.rs` produces a `multipart/mixed` message containing the attachment part (asserted via `MockGmailClient` — PREVIEW performs zero client I/O).
- The new cookbook recipe in `docs/cookbook/gmail.md` passes the `cookbook_skills.rs` parse-check ratchet.
- Patch version bumped in `packages/qfs/crates/qfs/Cargo.toml`.

**Verification method** — the commands/tests/probes that prove them (run from `packages/qfs`, `TMPDIR` redirected off the tmpfs, `command rm` for cleanup):

- `cargo build --workspace`, `cargo test --workspace` (incl. the new parser/eval/typeck and driver-gmail attachment tests), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`.
- `cargo run -p xtask -- gen-docs --check` and `cargo run -p xtask -- gen-skills --check` both green.
- **Live proof:** against the real Google account (token-import recipe from archived 203030), attach a **local** file to a draft and `CALL mail.send` with `--commit --commit-irreversible`; confirm the received message carries the attachment.

**Gate** — what must pass before approval:

- Full hermetic suite + both anti-drift `--check` gates green, AND the live local-file attach-and-send round-trip verified in-session.

## Considerations

- Closed-core discipline: the grammar addition must not leak driver types into `parser`/`core` (`domain-layer-separation`); drivers remain downstream of the frozen `EffectKind`.
- Keep the byte-literal syntax unambiguous with existing `Typed{ty,raw}` literals (`parser/src/grammar.rs`).
- Byte size: large attachments flow as `Value::Bytes` through `RowBatch` — sanity-check no accidental utf8 lossy conversion in `attachments_col` (`driver-gmail/src/effect.rs` ~289-329).
- Follow-on `20260701192440` needs a rows→Array aggregate to build the column *from a Drive read*; the literals here are the inline case only.
