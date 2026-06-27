---
created_at: 2026-06-26T09:30:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
commit_hash:
category:
depends_on: []
---

# Rework the future-UX 設計書 (docs/roadmap.md): gate every claim to live-verify, close the flow gaps

## Overview

`docs/roadmap.md` ("Where qfs is going") was just authored as the future-UX 設計書 — tiers, embedded
auth, DB-persistent architecture, cross-driver transactions, the Claude Code driver, multi-machine
tunnels, distributed scheduled jobs. It carries a `::: warning` banner saying it is a vision, not a
feature list, which is the right instinct. But by this repo's **standing honesty discipline** — the
same rule that drives `20260624214818-wire-real-execution-and-auth.md` and
`20260625165752-onboarding-mention-qfs-passphrase.md` ("never document a capability before it works;
keep the doc in lockstep with exactly what is live-verified") — the doc has two classes of problem:

1. **Live-verify honesty.** A single banner is not enough granularity. The doc mixes (a) things that
   are *shipped + live-verified today* (describe-is-pure → preview → commit → irreversible-needs-OK;
   `CREATE POLICY`; `CREATE TRIGGER`; local/sql/git/github/slack live commit), (b) things that
   *exist as libraries but are not yet wired/verified* (gmail/gdrive/ga OAuth, s3/r2 SigV4, cf
   parked), and (c) pure aspiration (tiers, tunnels, Claude Code driver, cross-driver atomic
   transactions, System/Project DB). A reader cannot tell which is which. The rework must mark each
   claim's status explicitly and anchor (a)/(b) to the real state recorded in the wire-real-execution
   tickets and RFD-0001 — not restate them as if uniformly real.

2. **Flow gaps.** The tier model and the new surfaces assert end states but skip the *transitions* —
   exactly the kind of sequencing gap that bit onboarding in `…onboarding-mention-qfs-passphrase.md`.
   Each flow below names an end state with no path, trust boundary, or identity/credential lifecycle.

**This is a docs/設計書 rework, not a code change.** Where a flow can't be made coherent without a
product decision (e.g. how the local `QFS_PASSPHRASE` vault relates to "sign-in"), the doc should
*flag the open decision*, not paper over it.

## Flow gaps to close (verified against the current docs/roadmap.md)

- **Local → Local+Cloud auth transition.** The doc says Local has "no auth — you are super-admin"
  and Local+Cloud makes "sign-in mandatory", but never reconciles this with the **existing** local
  credential vault: `QFS_PASSPHRASE`-derived (argon2id) encrypted `LocalStore` at
  `~/.config/qfs/credentials`, credential value via stdin (`account.rs`). What *is* "sign-in" on top
  of a vault that already exists? Define the transition or flag it as an open decision.
- **DB-persistent migration path.** The "System DB / Project DB" section introduces users +
  credentials + config as rows, but does not say what happens to today's file-based `LocalStore` /
  `.active` sidecar when a host adopts the DB. State the migration story (or mark TBD) — this is the
  same drift risk as before, one layer down.
- **Self-hosted invite flow.** "Invite by email / one-time signup URL → sign up to host or via qfs
  Cloud OIDC" names endpoints but no steps, no trust boundary, and no mapping from an invited
  identity to `POLICY` scopes (the existing access-control primitive). Spell out enrol → identity →
  policy, or flag the gaps.
- **Managed-team OAuth.** "OAuth already wired, no GCP client to register" implies qfs Cloud holds
  delegated OAuth on the user's behalf — a significant trust/consent boundary the doc skips. Tie it
  to `docs/security/threat-model.md` and state the consent/scope-delegation model (or mark TBD).
- **Multi-machine tunnels.** The cloudflared-style "one identity, many machines" flow omits machine
  enrolment, how identity binds across machines, and **revocation**. Add the lifecycle.
- **Claude Code driver across the tunnel.** The "ask my laptop to check my office" scenario does
  `SELECT`/`INSERT` against `/claude/<machine>/sessions` over a tunnel with no stated authz — which
  verbs, under which `POLICY`, with what preview/commit semantics. Close it or flag it.
- **Dashboard/CLI parity.** Asserted as a constraint but never *shown* as a flow (click → composed
  qfs statement → preview → commit). Add the one concrete flow that makes the parity claim legible.

## Scope — what to change

- **`docs/roadmap.md`** (primary): introduce a per-claim **status convention** (e.g. a small legend
  — Shipped & verified / Built, not wired / Proposed) and apply it section by section; ground the
  Shipped/Built rows in the wire-real-execution ticket state + RFD-0001; rewrite each flow above to
  include its transition, trust boundary, and identity/credential lifecycle, flagging open product
  decisions explicitly rather than implying resolution.
- **Cross-link, don't duplicate.** Point at `docs/security/threat-model.md`, `docs/guide/accounts.md`
  (the `QFS_PASSPHRASE` vault), `docs/server.md` (POLICY/TRIGGER), and the RFD for anything already
  documented, instead of re-asserting it in the roadmap.
- **Honesty parity with the agent surface.** If any roadmap claim is mirrored into
  `plugins/qfs/skills/qfs/SKILL.md` or the install welcome, keep them consistent (they were the exact
  drift vectors in the prior two tickets).

## Key files

- `docs/roadmap.md` (the 設計書 under rework), `docs/.vitepress/config.mts` (nav/sidebar entry,
  already added — leave intact).
- Anchors to cite, not edit blindly: `.workaholic/RFDs/0001-qfs-architecture.md`,
  `docs/security/threat-model.md`, `docs/guide/accounts.md`, `docs/adr/0005-deployment-hosts.md`,
  and the sibling tickets `20260624214818-wire-real-execution-and-auth.md` (the live-verify status of
  record) and `20260625165752-onboarding-mention-qfs-passphrase.md` (the honesty precedent).

## Considerations

- **Anti-drift (CLAUDE.md):** `docs/{language,drivers,server}.md` are gen-docs goldens — do NOT
  hand-edit; `roadmap.md` is hand-authored prose and is the right place for this work.
- **Honesty first:** the deliverable is a 設計書 a reader can trust line-by-line about what is real
  today vs. proposed — the same standard the rest of this ticket set enforces on shipped docs.
- **Decisions vs. prose:** prefer flagging an open product decision (sign-in-vs-vault, OAuth
  delegation, tunnel revocation) over inventing a flow that later has to be unwound.
- **Versioning:** a docs-only prose change. If treated as a shipped PR, bump the patch in
  `packages/qfs/crates/qfs/Cargo.toml` per CLAUDE.md; no `gen-docs` regen required (no golden
  touched).
- **Verify:** `docker compose up docs` and read `/roadmap` end-to-end — every capability claim should
  carry a visible status, and every flow should name its transition + trust boundary or an explicit
  TBD. No bare future capability presented as if it works today.
