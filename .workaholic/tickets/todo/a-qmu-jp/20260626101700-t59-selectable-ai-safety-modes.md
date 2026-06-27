---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626101000-t52-preview-commit-approval-cards.md, 20260626100800-t50-bearer-refresh-token-mcp-auth.md]
---

# t59 — Selectable AI safety modes (3 presets)

## Overview

Delivers **decision J** and roadmap §2.4 within milestone **M5**: the agent commit boundary
becomes a **selectable operator setting** with three presets — **Autonomous-in-policy** (default:
reversible auto-commits within `POLICY`, irreversible needs human approval), **Approve-everything**
(human approval for both), and **Policy-only** (both within `POLICY`, for CI/unattended). The
mechanism this builds on already exists as a library: `crates/core/src/security.rs` ships
`IrreversibleGuard::require_ack(plan, mode, ack)`, `RunMode`, and `NeedsPreview`, and
`crates/plan` already classifies irreversibility (`Plan::is_irreversible()`,
`EffectNode.irreversible`, `is_inherently_irreversible()`). The dashboard approval card exists
after t52, and bearer-token→user→policy mapping exists after t50. What is genuinely **new**: the
three-way mode as a first-class operator setting (stored as data), the mapping of each mode onto
the existing reversible/irreversible × within-policy decision, and the hook at the commit boundary
that routes an irreversible plan to the t52 approval card / push notification (except in
Policy-only). This is the **floor** of decision J, not a new safety model — it makes the existing
floor *configurable* without weakening it.

## Exact seams

- `crates/core/src/security.rs` — `IrreversibleGuard::require_ack(plan, mode, ack)`, `RunMode`,
  `NeedsPreview`. The **new** 3-preset mode parameterizes this: extend/wrap `RunMode` (or add a
  `SafetyMode` enum the guard consults) so the same `require_ack` call yields different outcomes
  per preset. Keep the function pure — it classifies, it does not perform the approval.
- `crates/plan/src/plan.rs` `Plan::is_irreversible()` / `topo_order`; `crates/plan/src/node.rs`
  `EffectNode.irreversible`, `EffectKind` (Remove/Call/...), `is_inherently_irreversible()` —
  the per-plan/per-node irreversibility classification the mode routes on. Source of truth; do
  not re-derive irreversibility elsewhere.
- `crates/server/src/policy/gate.rs` `gate_plan`/`resolve_policy` + `enforce.rs`
  `evaluate(policy, plan)` (pure, default-deny, extended in t57) — "within `POLICY`" means: the
  plan passes the policy gate. The safety mode composes *on top of* the policy decision
  (`within-policy ∧ mode`), never replaces it. A plan denied by policy is denied regardless of
  mode.
- `crates/runtime/src/interpreter.rs` `Interpreter::preview/commit/commit_txn` — the commit
  boundary where the mode decision is enforced: reversible-within-policy auto-commits; an
  irreversible plan in Autonomous/Approve modes is suspended pending approval.
- t52's preview→commit approval card (`crates/plan` `preview` output rendered visually) — where
  an irreversible plan goes for a human one-time ack in Autonomous/Approve modes; the card is the
  *same* preview→commit the CLI shows. The mode picks whether the card is raised at all.
- t50 bearer→user→policy mapping — the actor whose policy + mode apply to an MCP `commit` call;
  the mode is resolved per actor/deployment, not hardcoded.
- Mode persistence as **data**: the operator setting is a row in the System DB, surfaced as a
  `/sys/*` path (t53 `qfs-driver-sys`, e.g. `/sys/settings` or under `/sys/policies`), so it is
  itself describable/previewable/committable — preserving one-engine-three-faces.

## Implementation steps

1. **Model the 3 presets (pure, green).** Add a `SafetyMode { AutonomousInPolicy, ApproveEverything,
   PolicyOnly }` enum and a pure decision function
   `decide(mode, plan_is_irreversible, within_policy) -> {AutoCommit | NeedApproval | Deny}`.
   Exhaustive-match the 3×2×2 table from roadmap §2.4. Unit-test every cell; no I/O.
2. **Thread the mode through `security.rs`.** Wire `SafetyMode` into
   `IrreversibleGuard::require_ack`/`RunMode` so the existing guard call returns the
   step-1 decision; keep it pure and keep the default = Autonomous-in-policy. Update
   `security.rs` unit tests.
3. **Persist the setting + `/sys` surface.** Store the active mode in the System DB (new t42
   migration) and expose it via t53's `/sys/*` path so `FROM /sys/settings` reads it and an
   `INSERT/UPDATE` sets it (policy-gated, super-admin only). Round-trip test.
4. **Enforce at the commit boundary.** In `crates/runtime/src/interpreter.rs`'s commit path,
   apply the resolved decision: auto-commit AutoCommit; suspend NeedApproval and raise the t52
   approval card / push notification; never raise a card in Policy-only (auto within policy for
   CI). Map an MCP `commit` (t50) actor → mode. Integration test all three presets end-to-end.
5. **Honest docs + version.** Document the 3 modes in `docs/` (the §2.4 table) and the skill only
   once enforced end-to-end. Bump patch in `crates/qfs/Cargo.toml`; run
   `cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/core/src/security.rs` — `SafetyMode`, the pure `decide(...)`, threaded through
  `IrreversibleGuard`/`RunMode`.
- `crates/runtime/src/interpreter.rs` — commit-boundary enforcement + approval suspension.
- `crates/server/src/policy/gate.rs` — "within policy" feeds the mode decision.
- New System-DB migration (active mode) + t53 `/sys` path exposing the setting.
- `crates/qfs/src/serve.rs` / commit wiring — resolve actor→mode; raise t52 card / notification.
- `docs/server.md` / `docs/language.md` are generated — change the source, regenerate via xtask.

## Considerations

- **This configures the floor, it never lowers it.** The qfs safety floor (describe pure, preview
  touches nothing, commit explicit, irreversible needs an extra ack) stays intact in every mode.
  Even **Policy-only** does not bypass `POLICY` — it removes the *human* approval step for
  irreversible effects *only when policy already allows them*, for unattended CI. A plan outside
  policy is still denied in all three modes. Make that impossible to misread in code and docs.
- **Pure decision, impure approval.** Keep `decide(...)`/`require_ack` pure (they classify);
  the only impure steps are committing and raising the approval card / push notification. This
  keeps preview-as-CI able to show "would need approval" with no live creds.
- **Single source of truth for irreversibility.** Route exclusively on `crates/plan`'s
  `Plan::is_irreversible()`/`EffectNode.irreversible`/`is_inherently_irreversible()` — never
  re-classify a `CALL`/`REMOVE` ad hoc in the runtime, or the safety guarantee forks.
- **Default safety.** Default = Autonomous-in-policy; an unset/misconfigured mode must fail to the
  *safest sensible* default (treat as Autonomous-in-policy, i.e. irreversible needs approval),
  never to Policy-only-auto. Fail safe, not open.
- **Approval routing.** Reuse t52's card (same preview→commit, rendered visually); the
  push-notification channel is a separate transport — flag its delivery mechanism (and the
  approval-token/expiry shape) as an item to settle, do not invent a bespoke one here.
- **Dep-direction & purity.** Mode model + decision live in pure cores (`crates/core`/
  `crates/plan`/`crates/server`); enforcement is in `qfs-runtime`; wiring on the binary leaf.
  No tokio in the pure cores; `qfs-cmd` stays clean.
- **Versioning.** One PR + patch bump in `crates/qfs/Cargo.toml` + `v0.0.x` tag on ship.
