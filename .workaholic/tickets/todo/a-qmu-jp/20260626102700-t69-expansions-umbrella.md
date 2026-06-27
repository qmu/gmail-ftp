---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626102200-t64-claude-driver.md, 20260626101500-t57-extended-policy-acl.md]
---

# t69 — Expansions umbrella (CDC, driver SDK, credential brokering, approvals, observability, agent mesh)

## Overview

This is the **M+ umbrella** for roadmap Part 5 — "capabilities the foundation makes cheap." Every
item below is a **candidate, NOT a commitment**: each is gated on demand, and each would get its own
full ticket(s) (matching the depth of the existing tickets) before any work starts. This document
exists to record the *seam* each expansion would attach to, so the foundation is built without
foreclosing them — it is a map, not a work order. Nothing here is scheduled, and nothing here should
be documented, advertised, or counted as planned in `docs/`, the skill, or the README until a real
ticket is cut and a slice ships (honesty-first). Each sub-effort is listed with: what it is, the
exact existing seam it extends, and why it is cheap given the architecture — and explicitly framed as
optional.

## Exact seams (per candidate — each is a future, separate ticket)

- **Change subscriptions / CDC (candidate).** Turn `TRIGGER` from poll → push. Extends
  `crates/watchtower/` — today `Watcher`/`WatcherStore` poll and diff; a push path would add a
  webhook-or-stream-backed subscription so `/mail`, `/github`, `/slack` push changes. The
  `WebhookBinding` (HMAC-SHA256 verify), `EventBus`/`LocalBus`, `Dispatcher`, `bind_new`, and the
  at-least-once dedup ledger already exist; CDC is a new `EventKind` source feeding the same bus, not
  a new engine. *Future ticket of its own.*
- **Driver SDK + signed registry (candidate).** The closed-core/open-registry split already invites
  community drivers. A published SDK would formalize `crates/driver/src/lib.rs` `pub trait Driver`
  (the template is `crates/driver-local`) as a stable external API; a signed registry would verify
  driver provenance (reuse `crates/crypto-core` `sha256`/`hmac_sha256`/`constant_time_eq` for
  signature verification). Governance constraint that must hold: a community driver adds ZERO keywords
  (`crates/lang/src/keywords.rs` frozen at 38) — paths/procs/codecs only. *Future ticket(s) of its
  own.*
- **Short-lived credential brokering (candidate).** Instead of long-lived `connections`, mint
  per-plan, per-scope tokens that expire at commit — least privilege to its limit. Extends the
  `crates/secrets/src/store.rs` `Secrets` seam + the `resolve()` ladder
  (`crates/secrets/src/resolve.rs`): a broker would resolve a scoped, short-TTL token bound to a
  specific `Plan`'s effects (`crates/plan/src/plan.rs` `Plan`), reusing the t43 envelope store for
  the wrap and `Secret` redaction/zeroization. The t66 brokering work is the nearest precedent.
  *Future ticket of its own.*
- **Approval workflows as data (candidate).** Generalize the selectable safety mode (decision J,
  t59) to multi-party approval: an irreversible plan becomes a row in `/sys/approvals` that a second
  human signs off. Extends the t53 `qfs-driver-sys` `/sys/*` surface (pattern:
  `crates/server/src/driver.rs` `ServerDriver`) and hooks the commit boundary already built on
  `crates/core/src/security.rs` `IrreversibleGuard`/`RunMode`/`NeedsPreview`. The approval is itself a
  previewable/committable `/sys` mutation — "everything is a path." *Future ticket of its own.*
- **Observability as paths (candidate).** Expose metrics/traces at `/sys/metrics` so operating a
  fleet uses the same grammar as everything else. A new read-only `/sys/metrics` node on the t53
  `qfs-driver-sys` surface, schema defined in `crates/core/src/ddl/server.rs`'s `/sys` analogue. Pure
  read, no new keyword. *Future ticket of its own.*
- **Agent mesh (candidate).** With `/claude/...` across machines (t64) reachable over the t63
  tunnel, a coordinator agent on one host fans work to agents on others and collects results —
  multi-agent orchestration expressed in qfs. Builds entirely on the t63 relay + t64 `/claude/...`
  driver + `crates/server/src/policy/enforce.rs` `evaluate` (every cross-machine call still
  default-deny gated); the mesh is a usage pattern + a coordinator, not new transport. *Future
  ticket(s) of its own.*

## Implementation steps

This umbrella ships **no code**. Its only deliverable is this recorded map of seams plus, optionally,
keeping the foundation honest as candidates are picked up:

1. Keep this file as the Part-5 candidate ledger; when a candidate is chosen, cut a dedicated ticket
   (full Overview / Exact seams / Implementation steps / Key files / Considerations, matching the
   existing tickets) and link it back here. Do NOT implement from this umbrella directly.
2. Each future candidate ticket follows the standard slice discipline (each slice leaves
   `cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check` green) and gets its own
   PR + patch bump + `v0.0.x` tag.
3. Until a candidate's slice ships, it stays out of `docs/`, the skill (`crates/skill/assets/SKILL.md`),
   and the README — these are candidates, not roadmap promises.

## Key files

- This ticket file (the candidate ledger). No source files are created or modified by the umbrella
  itself; each candidate names its own seam (above) when its real ticket is written.

## Considerations

- **Candidates, not commitments — say it plainly.** Roadmap Part 5 is explicit that these are "what
  the foundation makes cheap," not scheduled features. This ticket records seams so the foundation
  does not foreclose them; it authorizes no implementation. Nothing here is a plan-of-record until a
  dedicated ticket exists.
- **Safety floor applies to every candidate.** Each, when built, inherits the floor: describe pure,
  preview touches nothing, commit explicit, irreversible needs the extra acknowledgement. CDC must
  stay at-least-once + idempotent (the watchtower contract); credential brokering must keep
  `Secret` redaction; approvals route through `IrreversibleGuard`; the mesh re-checks `POLICY` per
  cross-machine call.
- **Governance constraints survive expansion.** The driver SDK + registry must preserve the closed
  core — ZERO keywords from community drivers (`crates/lang/src/keywords.rs` frozen at 38); the
  dep-direction rules (`crates/cmd/tests/dep_direction.rs`) still apply to any new leaf, and tokio
  stays in the binary leaf.
- **Honesty-first.** Do not advertise any candidate as a capability before its real slice ships;
  this umbrella must not leak into generated docs.
- **Versioning:** the umbrella itself ships no version bump (no code); each candidate's eventual
  ticket carries its own PR + patch bump + `v0.0.x` tag.
