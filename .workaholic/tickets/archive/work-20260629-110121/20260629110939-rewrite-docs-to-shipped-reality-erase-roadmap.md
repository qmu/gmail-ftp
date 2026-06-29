---
created_at: 2026-06-29T11:09:39+09:00
author: a@qmu.jp
type: housekeeping
layer: [UX, Config]
effort: 4h
commit_hash: b7e32aa
category: Changed
depends_on: []
---

# Rewrite the docs to v0.0.9 reality and erase the roadmap vision doc

## Overview

`docs/roadmap.md` was authored as the future-UX **設計書** ("Where qfs is going") — a status-tagged
vision (✅ shipped / 🔌 built-not-wired / 🧭 proposed) over an M0→M+ phased plan. That plan is **no
longer the future**: the night `/drive` shipped the **entire roadmap (t42–t81, M0–M9 plus M+)** and
released **v0.0.9** (PR #10 merged to `main`, GitHub Release published). Most of what the roadmap
framed as "aspiration" — SQLite System/Project DB, human identity, OAuth AS / DCR / PKCE, the MCP
endpoint, the embedded SPA dashboard with approval cards, the `/sys/*` admin paths, teams/invites,
selectable AI safety modes, the lowercase M6 language, the new drivers, billing tiers — is now
**actually shipped**. A vision document describing reality as a future plan is exactly the kind of
drift the repo's **standing honesty discipline** forbids (the through-line of the onboarding +
sekkeisho tickets: *never document a capability before it works; keep every doc surface in lockstep
with exactly what is live-verified*).

This ticket **comprehensively rewrites the hand-authored prose docs to describe what qfs actually
ships today (v0.0.9)** and **erases `docs/roadmap.md`** in the same change, folding its now-true ✅
content into the real docs and **dropping the still-unwired 🧭 seams** rather than carrying them as
promises. It is a docs/housekeeping change — **no product code changes**. (It supersedes the
narrower, already-archived `…sekkeisho-rework-live-verify-and-flow-gaps.md`, which wanted to *gate*
roadmap claims in place; that disposition is now moot — the page goes away.)

The discipline **cuts both ways post-v0.0.9**: the docs must now describe the shipped reality, but
the **still-unwired seams MUST NOT be claimed as working** — keep them out of the prose (or note them
honestly as "not yet wired"): live OAuth browser consent, the MCP cloud tunnel dial, the
LDAP/AD/Entra/Workspace directory, the qfs Cloud broker endpoint, the payment provider,
Postgres/MySQL SQL backends (SQLite ships), gmail/gdrive/ga/objstore **reads** (github/slack reads
ship), and the CF Workers wasm artifact (parked per ADR-0005 / CLAUDE.md).

## Exact seams

**ERASE + de-link (the roadmap and its references):**
- `docs/roadmap.md` (≈1042 lines) — **delete**. Fold the now-true ✅ capabilities into the prose docs
  below; drop the 🧭 proposed seams.
- `docs/.vitepress/config.mts` — remove the two `/roadmap` links: the **nav** entry
  (`{ text: 'Roadmap', link: '/roadmap' }`) and the **sidebar** "Roadmap" group (which also nests
  `/query-cookbook`). **Re-home `query-cookbook` in the sidebar** (it survives — worked recipes).
  Update the outline-level comment that justifies `level: [2,3]` "so the roadmap's `## Part N`
  headers appear". NOTE `ignoreDeadLinks: true` is set, so a stale `/roadmap` link will **not** fail
  the VitePress build — dead links must be hunted manually (`grep -rn roadmap docs/`).
- `docs/query-cookbook.md` — scrub the `[roadmap](/roadmap)` link + the `§4.2/§4.4` roadmap
  cross-references (≈ lines 10, 382, 397); re-anchor the federation/architecture explanation in-page
  or to `guide/concepts.md`. (Its enforcing test `…/test/tests/roadmap_cookbook.rs` keeps its name —
  a code rename is **out of scope** for this docs ticket; flag it only.)

**REWRITE to v0.0.9 reality (hand-authored prose — the legitimate edit surface):**
- `README.md` (repo-root = the qfs README) — refresh the driver list (it omits the shipped
  `claude`/`directory`/`cf`/`http` drivers), the deploy story (wasm is parked, not "(target)"), and
  add the now-shipped surfaces: identity, teams/invites, MCP endpoint, dashboard, `/sys`, OAuth AS,
  jobs, billing.
- `docs/index.md` — refresh the hero/features so the **three faces (CLI · dashboard · MCP)** read as
  shipped today.
- `docs/guide/getting-started.md`, `installation.md`, `cli.md`, `concepts.md`, `connections.md`,
  `shell.md` — rewrite to the actual shipped loop, install path (install.sh + GitHub Release
  tarballs), and architecture (one engine / three faces; System/Project SQLite DB; safety floor +
  selectable safety modes; `/sys` as ordinary paths).
- `docs/cookbook/{index,mail,databases,files,cross-service,code,automation}.md` — rewrite to v0.0.9
  syntax/verbs; `automation.md` most needs the new `qfs job run/cron` + dashboard reality.
- `docs/security/threat-model.md` — rewrite to the shipped security surfaces: OAuth AS / bearer +
  refresh / MCP auth, sessions, hash-chained WORM audit, per-recipient E2E DEK wrap, credential
  rotation/revocation, default-deny policy gate, extended ACL.
- `packages/qfs/crates/skill/assets/SKILL.md` + `plugins/qfs/skills/qfs/SKILL.md` — rewrite to the
  full shipped surface (MCP, `/sys`, safety modes, teams), keeping the two copies in sync; drop any
  roadmap-as-future framing.

**ANTI-DRIFT — do NOT hand-edit (CLAUDE.md, CI-gated):**
- `docs/{language,drivers,server}.md` are **generated** from the binary (`qfs::docs` via `xtask
  gen-docs`) and carry no roadmap refs. **Do not touch them.** Only `cargo run -p xtask -- gen-docs
  --check` to confirm they are in sync; if reality requires a change, edit the Rust source and
  regenerate.

**SOURCE-OF-TRUTH references (read to ground every claim — not edited):**
- `packages/qfs/crates/cmd/src/lib.rs` — the real CLI verbs: `qfs connection
  {add,list,use,remove,rotate,revoke,rekey}`, `qfs identity {signup,whoami}`, `qfs invite
  {create,redeem,revoke}`, `qfs job {run,cron}`, plus `run`/`describe`.
- `packages/qfs/crates/driver-sys/src/schema.rs` — the `/sys/*` paths
  (`users/projects/audit/connections/policies/metrics/settings/billing`).
- `packages/qfs/crates/lang/src/keywords.rs` — the frozen lowercase keyword set + `==`(compare)/
  `=`(bind); every rewritten example must use canonical lowercase + correct operator.

**RELATED CLEANUP (flag/decide):**
- `ROADMAP-TICKETS.md` (repo **root**, not `docs/`) — its header says "40 tickets drafted (todo) —
  t42…t81 … current binary is 0.0.8"; all 40 shipped in v0.0.9. **Delete it alongside roadmap.md**
  (recommended — it is the index of the now-shipped roadmap), or flip its status to "all shipped";
  decide and do it.
- `.workaholic/tickets/README.md` — its "Ready for `/report` + `/ship`" line is stale post-ship.
  Minor; this file lives under `.workaholic/tickets/` which a hook (`guard-ticket-structure.sh`)
  guards — edit with the **Edit tool, not shell**, and tread lightly (or leave it).

## Implementation steps

1. **Audit reality first.** Read the source-of-truth files (cmd `lib.rs`, `driver-sys/schema.rs`,
   `lang/keywords.rs`) + the shipped surfaces, and grep `roadmap` across the repo to enumerate every
   reference. List, per capability, whether it is **shipped** (document it) or a **seam** (omit /
   note honestly). Tree must stay green throughout.
2. **Rewrite the prose docs** (README, index, guide/*, cookbook/*, threat-model, both SKILL.md) to
   v0.0.9 reality — grounded in the source-of-truth, canonical lowercase syntax, three-faces framing,
   the new commands/drivers/paths. Keep the two SKILL.md copies in sync.
3. **Fold roadmap content + erase.** Move the ✅ now-true architecture/decisions into
   `concepts.md`/`threat-model.md`/`connections.md`, then **delete `docs/roadmap.md`**; drop the 🧭
   seams.
4. **De-link.** Remove the `/roadmap` nav + sidebar entries from `config.mts` and re-home
   `query-cookbook`; scrub the `/roadmap` links + §refs from `query-cookbook.md`; `grep -rn roadmap
   docs/ README.md` to confirm **no dangling links** (the build won't catch them — `ignoreDeadLinks`).
5. **Related cleanup.** Delete-or-update `ROADMAP-TICKETS.md`; optionally refresh
   `.workaholic/tickets/README.md` via the Edit tool.
6. **Verify + version.** `cargo run -p xtask -- gen-docs --check` (generated docs in sync,
   untouched); `cargo test --workspace` (incl. the `roadmap_cookbook.rs` parse-coverage ratchet still
   green); `cargo fmt/clippy` green. Bump the patch in `packages/qfs/crates/qfs/Cargo.toml`
   (**0.0.9 → 0.0.10**) per CLAUDE.md. Optionally `docker compose up docs` to eyeball the rendered
   nav.

## Key files

- **Erase/de-link:** `docs/roadmap.md` (delete), `docs/.vitepress/config.mts`,
  `docs/query-cookbook.md`, `ROADMAP-TICKETS.md` (repo root).
- **Rewrite:** `README.md`, `docs/index.md`, `docs/guide/{getting-started,installation,cli,concepts,
  connections,shell}.md`, `docs/cookbook/{index,mail,databases,files,cross-service,code,
  automation}.md`, `docs/security/threat-model.md`, `packages/qfs/crates/skill/assets/SKILL.md`,
  `plugins/qfs/skills/qfs/SKILL.md`.
- **Reference only (source of truth):** `packages/qfs/crates/cmd/src/lib.rs`,
  `packages/qfs/crates/driver-sys/src/schema.rs`, `packages/qfs/crates/lang/src/keywords.rs`.
- **Do NOT edit (generated):** `docs/{language,drivers,server}.md` (verify via `gen-docs --check`).
- **Version:** `packages/qfs/crates/qfs/Cargo.toml` (0.0.9 → 0.0.10).

## Considerations

- **Anti-drift (hard, CI-gated, CLAUDE.md).** `docs/{language,drivers,server}.md` are rendered from
  the binary — never hand-edit; the change touches only hand-authored prose + the vitepress nav, and
  proves the generated set is in sync with `gen-docs --check`. This keeps the layer `Config` work
  (nav) and `UX` work (prose) cleanly separated from the generated plane.
- **Honesty discipline cuts both ways.** Now that M0–M+ shipped, the docs must describe the real
  v0.0.9 surface — but the unwired seams (listed in the Overview) must NOT be claimed as working.
  Live-verify every capability claim against the binary + RFD-0001; do not restate old roadmap prose
  as fact. Where a flow still has an open product decision (e.g. the live-OAuth-consent UX, the
  qfs-Cloud broker), say so plainly rather than papering over it.
- **Cross-link, don't duplicate.** Point pages at `guide/connections.md`, `security/threat-model.md`,
  the generated `server.md`/`drivers.md`, and RFD-0001 rather than re-asserting the same facts in
  many places — the erased roadmap's job of being the single narrative is replaced by a coherent,
  cross-linked guide set, not one mega-page.
- **Dead links won't fail the build.** `ignoreDeadLinks: true` in `config.mts` means a missed
  `/roadmap` reference ships silently — the only guard is a manual `grep`. Make that grep a gate of
  the implementation.
- **`query-cookbook` survives; `roadmap_cookbook.rs` keeps its name.** The cookbook is worked recipes
  (still valuable) — re-home it in the nav, don't delete it. Its enforcing test file is named
  `roadmap_cookbook.rs`; renaming it is a code change out of this docs ticket's scope — flag it as a
  follow-up candidate, don't do it here.
- **Open decision to make in-ticket (not guess):** whether `ROADMAP-TICKETS.md` is deleted (it
  indexes the now-shipped roadmap — recommended) or downgraded to a "shipped" changelog. Pick one and
  do it consistently with the `docs/roadmap.md` erase.
- **Versioning.** A shipped docs PR bumps the patch (0.0.9 → 0.0.10) and cuts a matching `v0.0.10`
  tag on ship, per CLAUDE.md — even though no `gen-docs` regen is expected.
