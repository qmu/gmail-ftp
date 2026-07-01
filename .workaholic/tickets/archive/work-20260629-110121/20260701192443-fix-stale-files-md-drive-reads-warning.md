---
created_at: 2026-07-01T19:24:43+09:00
author: a@qmu.jp
type: bugfix
layer: [Config]
effort:
commit_hash: 9f92a75
category: Changed
depends_on:
---

# Fix stale docs/cookbook/files.md Drive-reads/writes warning

## Overview

**Sub-ticket of EPIC `.workaholic/tickets/todo/a-qmu-jp/20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`.** `docs/cookbook/files.md` carries a warning box (lines ~146-152) that is **provably stale**: it says *"**Drive reads are coming soon** (path→id resolution is still being wired)"* and that a `/drive` write only *"previews today."* Both are contradicted by the shipped code — `driver-gdrive/src/read.rs` implements the name→id walk, content download (`content_batch`, ~192-204), and Google-native export, and `client.rs::upload`/`update_content` perform real Drive writes (live-verified in archived ticket 203030). The migration guide (`docs/guide/replace-gmail-gdrive-ftp.md`) already reflects the wired reads, so `files.md` is the lone stale spot.

This is the smallest, **immediately drivable** ticket (no code dependency — the features it documents already exist). Good "start earlier" quick win. It is **independent** of the other sub-tickets.

Experimental posture: straightforward doc correction; patch bump on the shipped PR.

## Policies

- `workaholic:implementation` / `policies/objective-documentation.md` — documentation must state what is verifiably true of the binary; the current box asserts a false capability status.
- **Anti-drift (CLAUDE.md):** `docs/cookbook/files.md` is the **authored source** for the `qfs-files` Agent Skill — after editing, run `cargo run -p xtask -- gen-skills` so `plugins/qfs/skills/qfs-files/SKILL.md` regenerates, and `gen-skills --check` stays green. Any qfs recipe shown must parse (`crates/test/tests/cookbook_skills.rs`). `docs/cookbook/coding-standards`/`directory-structure` are not relevant (no Rust changes).

## Key Files

- `docs/cookbook/files.md` - the stale warning box (lines ~146-152); correct the Drive reads/writes status. Keep the genuinely-still-true caveats (e.g. `/s3` and `/r2` writes returning `unsupported_verb`) accurate.
- `packages/qfs/crates/driver-gdrive/src/read.rs` - evidence that Drive reads (walk + `content_batch` + export) are wired.
- `packages/qfs/crates/driver-gdrive/src/client.rs` - evidence that `upload`/`update_content` perform real writes.
- `plugins/qfs/skills/qfs-files/SKILL.md` - generated output; must be regenerated, never hand-edited.

## Related History

- [20260630203030-google-live-verification-token-import.md](.workaholic/tickets/archive/work-20260629-110121/20260630203030-google-live-verification-token-import.md) - Live-verified Gmail and Drive reads against a real account — the proof that the `files.md` "reads coming soon" claim is stale.
- [20260630203040-gmail-gdrive-to-qfs-guidance-doc.md](.workaholic/tickets/archive/work-20260629-110121/20260630203040-gmail-gdrive-to-qfs-guidance-doc.md) - The migration guide that already documents reads as working (consistency target).

## Implementation Steps

1. Rewrite the `files.md` warning box: Drive reads (list, name→id resolve, content download, native export) are **implemented**; `/drive` writes (upload/update/trash/copy/move) apply on `--commit`.
2. Keep accurate the parts still true: a `/drive`/`/s3`/`/r2` read needs a connected account; `/s3` and `/r2` writes still return `unsupported_verb`.
3. Cross-check wording against the migration guide so the two docs agree.
4. Run `cargo run -p xtask -- gen-skills` to regenerate the `qfs-files` SKILL.md.

## Quality Gate

**Acceptance criteria:**

- `docs/cookbook/files.md` no longer claims Drive reads are "coming soon" or that Drive writes are preview-only; it accurately states Drive reads/writes are implemented and preserves the still-true `/s3`/`/r2` caveats.
- `plugins/qfs/skills/qfs-files/SKILL.md` is regenerated to match (not hand-edited).
- The doc and the migration guide agree on Drive capability status.
- Patch version bumped.

**Verification method** (from `packages/qfs`, `TMPDIR` redirected, `command rm`):

- `cargo run -p xtask -- gen-skills` then `cargo run -p xtask -- gen-skills --check` green (SKILL.md in sync).
- `cargo run -p xtask -- gen-docs --check` green.
- `cargo test --workspace` green (incl. `cookbook_skills.rs` parse-check of any recipe in the edited article).
- Manual read-through confirming no remaining stale claim.

**Gate** — `gen-skills --check` + `gen-docs --check` + `cargo test` green, and a manual diff review confirming the box now matches the wired behavior.

## Considerations

- No code change — purely documentation + regenerated skill artifact; safe to drive first/immediately.
- Do not hand-edit `plugins/qfs/skills/qfs-files/SKILL.md`; it must come from `gen-skills` (CLAUDE.md).
- Leave in place any caveat that is still accurate (object-store writes) — the goal is truth, not blanket optimism.
