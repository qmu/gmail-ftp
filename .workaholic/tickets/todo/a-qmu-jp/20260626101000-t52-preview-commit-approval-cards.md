---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260626100900-t51-embedded-spa-dashboard-shell.md, 20260626100800-t50-bearer-refresh-token-mcp-auth.md]
---

# t52 — preview→commit approval cards

## Overview
Delivers the heart of the dashboard face (roadmap §2.4, §"One engine, three faces", M3): the
**visual preview→commit loop**. The dashboard renders a posted qfs statement's effect-plan — the
per-effect rows and affected-count estimate produced by `crates/plan` `preview()` — as a
**preview card**, then offers a **commit card**; an irreversible effect (send mail, merge PR,
delete) raises a separate **one-time approval card** that maps to the exact extra acknowledgement
the CLI requires today (`--commit-irreversible`). This is the same `preview → commit` the CLI shows,
rendered visually — it adds NO new capability, it surfaces the existing one. What already exists as
a library: the entire pure dry-run summary (`crates/plan/src/preview.rs` `preview()` →
`Preview { rows, total_affected }`), the irreversibility model (`crates/plan` `EffectNode.irreversible`,
`Plan::is_irreversible()`, `is_inherently_irreversible()`), and the acknowledgement guard
(`crates/core/src/security.rs` `IrreversibleGuard::require_ack`, `RunMode`, `NeedsPreview`). What is
genuinely new: the JSON projection of a `Preview` for the browser, the commit-with-ack bridge
endpoint, and the three card UIs. The selectable safety modes that decide *whether* a card auto-
commits or must wait are t59; this ticket renders the card and wires the explicit-ack path.

## Exact seams
- `crates/plan/src/preview.rs` — `preview(plan) -> Preview`; `Preview { rows: Vec<PreviewRow>,
  total_affected: Affected }`; `PreviewRow { affected: Affected, ... }` in topological order. This
  is the SINGLE source of the "affected counts" the preview card shows — the browser gets a JSON
  projection of `Preview`, never a re-computed estimate. `preview()` is pure (no I/O).
- `crates/plan/src/plan.rs` — `Plan::is_irreversible()`, `topo_order`; `crates/plan/src/node.rs` —
  `EffectNode.irreversible`, `EffectKind` (`Remove`/`Call`/…, non_exhaustive),
  `is_inherently_irreversible()` (Remove=true; Call per-proc). These decide which card a row gets:
  reversible → commit card; irreversible → approval card.
- `crates/core/src/security.rs` — `IrreversibleGuard::require_ack(plan, mode, ack)`, `RunMode`,
  `NeedsPreview`. The approval card's "Confirm" posts an explicit ack that flows through THIS guard
  — the same gate the CLI's `--commit-irreversible` flag drives. No second irreversibility check.
- `crates/exec/` — `qfs_exec::{build_plan, execute_read}`, `run_oneshot`/`apply_commit`. The commit
  card posts to a bridge that calls `apply_commit` (the one engine path), not a dashboard-local
  applier.
- `crates/qfs/src/dashboard.rs` (NEW in t51) — extend the `/api/run` bridge with `/api/preview`
  (returns the `Preview` JSON) and a gated `/api/commit` (takes the ack token); reuse t51's
  `http_core::HttpResponse` building and error sanitization.
- `crates/runtime/src/interpreter.rs` — `Interpreter::preview/commit/commit_txn` is the runtime
  bridge the binary already uses; the commit endpoint routes through it so dashboard commits share
  the CLI's frontier-parallel/auto-batch behavior.
- Auth: t50 bearer/refresh token validation guards `/api/commit` (the dashboard call carries the
  session→bearer mapping); preview may be read-gated by session (t46) but commit MUST be token-
  guarded (t50).
- `crates/http-core/src/lib.rs` `is_sensitive_header`/`SENSITIVE_HEADERS` — the card payloads and
  any echoed request metadata are redaction-aware; affected-count summaries are secret-free by
  construction (`Preview` is "deterministic, secret-free" per the preview.rs module doc).

## Implementation steps
1. **Preview projection (pure, tree green).** Add a serde JSON projection of `crates/plan`
   `Preview`/`PreviewRow`/`Affected` (a thin DTO in the bridge, NOT a leak of plan internals across
   crates). Extend `crates/qfs/src/dashboard.rs` `/api/preview` to call `build_plan` →
   `crates/plan::preview` and return it. No commit, no card UI yet — `cargo build/test/clippy/fmt`
   + `gen-docs --check` green.
2. **Preview card UI.** Render the projection in the SPA: one row per planned effect, the
   `total_affected` summary, and a clear reversible/irreversible badge derived from
   `EffectNode.irreversible`. Read-only/zero-effect plans show "reads only, 0 effects" (matching the
   roadmap §2.3 wording).
3. **Commit card + gated bridge.** Add `POST /api/commit` guarded by t50 bearer validation; for a
   reversible plan it calls `apply_commit` (the engine path) and returns the `CommitReport`. The
   commit card mirrors the CLI default: preview first, then an explicit commit action.
4. **Irreversible approval card.** When `Plan::is_irreversible()` / any `EffectNode.irreversible`,
   the UI raises a distinct one-time approval card; "Confirm" posts an explicit ack that the bridge
   feeds to `IrreversibleGuard::require_ack(plan, mode, ack)`. Without the ack the commit is refused
   with a structured "needs acknowledgement" response (same semantics as the CLI rejecting a missing
   `--commit-irreversible`). The mode comes from t59 (default mode if t59 not yet shipped: behave as
   Autonomous-in-policy — reversible auto-commit, irreversible requires the card).
5. **One-engine equivalence test + docs.** Golden test: the same statement yields the same `Preview`
   whether requested by the CLI, the t47 MCP `preview` tool, or `/api/preview` (assert the plan/
   preview, not the rendering). Flip the roadmap §2.4 status tag only after the card fires a real
   gated commit. Patch bump in `crates/qfs/Cargo.toml`.

## Key files
- `crates/qfs/src/dashboard.rs` — add `/api/preview` and gated `/api/commit`; the `Preview` JSON DTO.
- `crates/qfs/assets/dashboard/{app.js,app.css}` — preview card, commit card, approval card.
- `crates/plan/src/preview.rs` — consume only; do NOT change the pure summary's shape unless a
  field is genuinely missing for the card (flag it if so).
- `crates/core/src/security.rs` — consume `IrreversibleGuard`/`RunMode`; no new guard.
- `crates/qfs/Cargo.toml` — patch bump.
- `docs/roadmap.md` — flip the preview→commit-cards status tag once it fires (honesty rule).

## Considerations
- **Safety floor is the whole point.** The cards must not weaken any step: describe pure / preview
  touches nothing / commit explicit / irreversible needs an extra ack. The approval card is a *UI
  rendering of the existing ack*, routed through `IrreversibleGuard::require_ack` — never a new,
  looser path. A reversible auto-commit and an irreversible held-for-approval must match what the
  CLI does for the same plan (one-engine constraint), which the equivalence test pins.
- **No re-computation of effects.** Affected counts come only from `crates/plan` `preview()`; the
  browser never estimates. `Preview` is secret-free by construction, so the card payload carries no
  credentials and no raw upstream data beyond the plan summary.
- **Token-guarded commit.** `/api/commit` is the one mutating dashboard endpoint and MUST sit behind
  t50 bearer validation mapping token→user→policy; a single-use/short-TTL ack token prevents an
  approval card from being replayed to fire twice. Preview/describe may be session-read-gated only.
- **Safety-mode coupling (flag, don't bake).** This ticket renders the card and the ack path; *which*
  effects auto-commit vs. wait is t59's selectable mode. Wire the mode as an injected `RunMode` and
  default to Autonomous-in-policy if t59 has not shipped — do not hard-code a single behavior.
- **Dep-direction.** Card bridge logic is live-runtime glue → `qfs` binary leaf only; the JSON DTO
  must not pull `qfs-cmd` toward plan/runtime crates. `crates/plan` stays I/O-free.
- **Idempotency/recovery.** A retried commit (network blip after the user clicks once) must converge,
  not double-fire: rely on the existing `UPSERT`/version semantics + the single-use ack token; document
  the at-least-once boundary on the card path.
- **Open product decision to flag:** whether the approval card supports *partial* approval (commit
  the reversible subset, hold the irreversible legs) or is all-or-nothing per statement. Prefer
  surfacing the existing plan granularity rather than inventing a new commit-splitting semantic;
  record the choice in the PR.
- **Versioning.** One PR, patch bump, `v0.0.x` tag on ship.
