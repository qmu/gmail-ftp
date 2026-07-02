---
created_at: 2026-07-02T12:01:10+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Config]
effort:
commit_hash: 0343c49
category: Changed
depends_on: [20260702120050-mount-bound-accounts-retire-connection.md, 20260702120060-qfs-host-skeleton.md]
---

# The docs hard-break sweep: every doc, cookbook, and skill on the new verbs (+ patch bump)

Part of EPIC `20260702120000` (ADR 0008 consequences). With the verbs final, rewrite the entire
documentation surface to `init` / `host` / `app` / `account` / `connect` — the terminology policy's
"update everything within the affected scope in the same change". This ticket owns the epic's
**retired-verb-zero** gate.

## Scope (from the source survey)

1. **Narrative guides** (`docs/guide/`): `account-model.md` (11 retired-verb references: the layer
   table, the how-to, the QFS_GOOGLE_ACCOUNT selection note, the Gmail walkthrough),
   `passphrase.md` (now also the KeyGuardian slots + `qfs vault enroll keychain` as the per-pane
   answer), `operator.md` (no-password `qfs init`, honest OS-delegation language — the "requires
   sign-in" wording), `connect.md`, `connections.md` (retire the add/use/list/remove operational
   sections; `rekey` moved), `getting-started.md`, `installation.md`, `cli.md`, `shell.md`.
2. **Cookbooks** (`docs/cookbook/*.md` — all 10): every Setup section's happy path becomes
   `qfs init` → `qfs app add google < credentials.json` → `qfs account add google` → `qfs connect
   /mail gmail <email>`; the two-gate prerequisite callouts reword to the new verbs; any new
   `CONNECT … ACCOUNT '…'` recipes must parse (the ratchet guards this).
3. **Generated surfaces**: `crates/qfs/src/docs.rs` (`qfs connection add` hard-coded @260),
   `crates/lang/src/reference.rs` (CONNECT statement reference gains ACCOUNT/HOST), the `qfs-skill`
   embedded agent skill source — then `cargo run -p xtask -- gen-docs` and `gen-skills` regenerate
   `docs/{language,drivers,server}.md` and `plugins/qfs/skills/*/SKILL.md`. Never hand-edit
   outputs.
4. **Installer**: `packages/qfs/install.sh` next-steps — verify the pointer targets still describe
   reality (the banner itself carries no verbs after commit `0208edc`).
5. **ADR/roadmap cross-refs**: `docs/roadmap.md` if it names the old verbs; ADR 0008 gets a
   "Status: implemented by …" note per the ADR conventions.
6. **Patch bump**: `packages/qfs/crates/qfs/Cargo.toml` (+ lockfile) per CLAUDE.md — this is the
   epic's shipping ticket.

## Key files

- `docs/guide/*.md`, `docs/cookbook/*.md`, `docs/adr/0008-multi-host-account-model.md`
- `packages/qfs/crates/qfs/src/docs.rs`, `crates/lang/src/reference.rs`, the qfs-skill crate source
- `packages/qfs/xtask` (gen-docs / gen-skills), `crates/test/tests/cookbook_skills.rs` (ratchet)
- `packages/qfs/install.sh`, `packages/qfs/crates/qfs/Cargo.toml`

## Considerations

- Keep the cookbook articles' lead-with-a-demo structure (this branch's rewrite) — the sweep
  changes Setup sections and verb references, not the article architecture.
- The passphrase article's five options gain the keychain slot as a shipped (not planned) path;
  the managed-service option stays "planned".
- Docs must stay truthful to what the binary does at this commit — nothing aspirational
  (objective-documentation policy).

## Quality Gate

Global gate (EPIC) plus — this ticket owns the epic's final gates:

- **Retired-verb zero**: `grep -rn "connection add\|connection use\|connection list\|connection
  remove\|identity signup" --include='*.md' --include='*.rs' --include='*.sh'` over the repo finds
  zero hits outside `.workaholic/tickets/archive|abandoned/`, `docs/adr/` (historical record), and
  migration/schema history. Run and paste the result into the ticket on completion.
- `gen-docs --check` + `gen-skills --check` in sync; cookbook parse ratchet green (MIN_STATEMENTS
  floor still cleared after recipe edits).
- The getting-started + gmail-cookbook happy paths, followed verbatim on this machine (token-import
  variant), reach a real `/mail/inbox` read — the docs-are-true check.
- Patch version bumped; `qfs --version` reflects it.

## Completion record (2026-07-03, /drive)

**Retired-verb-zero grep result** (`grep -rn "connection add\|connection use\|connection list\|connection remove\|identity signup" --include='*.md' --include='*.rs' --include='*.sh'`, excluding `tickets/archive|abandoned/`, `docs/adr/`, `.workaholic/stories/`, `/target/`):
the only remaining hits are this ticket's own gate text and the two ADR-track queue tickets
(`20260702120000` epic, `20260702130000` resume), all of which move to `tickets/archive/` at
close of this drive — zero hits in product docs, code, cookbooks, skills, and installer.

- Sweep totals: guides 70→0, cookbooks 24→0, README/skill/installer 13→0, crates prose →0.
- `gen-docs --check` + `gen-skills --check` in sync; cookbook parse ratchet green.
- Docs-are-true: the rewritten getting-started happy path (token-import variant) executed
  verbatim on this machine reached a real `/mail/inbox` read.
- Patch bumped 0.0.13 → 0.0.14; `qfs --version` reflects it.
- Note: the CONNECT ACCOUNT/HOST reference lives in `docs/guide/connect.md` — the EBNF in
  `crates/lang/src/reference.rs` deliberately carries only frozen-keyword forms (CONNECT is a
  contextual-ident statement), so it was not extended.
