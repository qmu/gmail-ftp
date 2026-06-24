# Review — t25 Defect-Fix Re-Review (Architect)

- Reviewer: Architect (Neutral / structural bridge)
- Scope: re-review of commit `14a1d96` ONLY (the two Planner-found defects + folded Architect carry-overs). The full driver was already reviewed at `736b6c7` (approve-with-minor) — not re-litigated here.
- Mode: analytical review only (no test/build/clippy execution).
- Decision: **Approve with observations**

## Files inspected

- `crates/driver-slack/src/effect.rs` (`swallows_already_done` widening)
- `crates/driver-slack/src/client.rs` (`is_already_done`, `BodyErrorRule::check`, `apply` dispatch — the consumers that make the widening sound)
- `crates/driver-slack/src/pushdown.rs` (`Lowered::{Exact, PreFilter}`, `lower`, `lower_cmp`)
- `crates/driver-slack/src/events.rs` (`SlackInbound::Unhandled`, `parse_event` dispatch)
- `crates/driver-slack/Cargo.toml` (`events` feature doc)
- `crates/driver-slack/src/tests.rs`, `tests/planner_e2e.rs` (new coverage)

## Fix 1 — Remove-side idempotency (the key correctness check): CORRECT

`swallows_already_done()` now matches `AddReaction | RemoveReaction | Pin | Unpin` (effect.rs:59-66).

The decisive point is that the swallow is a **two-condition AND**, not a single gate. In `BodyErrorRule::check` (client.rs:200):

```rust
if swallow_already_done && is_already_done(code) {
    return Ok(());
}
```

The widened selector only flips the **first** operand to `true` for the remove-side ops. The **second** operand, `is_already_done(code)` (client.rs:214-219), is unchanged and restricts the swallow to exactly `{already_reacted, already_pinned, no_reaction, not_pinned}`. Therefore:

- Symmetry with `is_already_done`: confirmed — the recognizer's code-set is exactly the four already-satisfied codes, one per side of each idempotent pair (add→`already_*`, remove→`no_reaction`/`not_pinned`). The selector now lists exactly the four ops whose error class those codes belong to. Selector and recognizer are in sync.
- No over-swallow of real failures: confirmed — a genuine remove error such as `message_not_found` on `reactions.remove` yields `is_already_done("message_not_found") == false`, so the AND fails and `Err(SlackError::Body{..})` (terminal) surfaces even with `swallow == true`. The new test `remove_side_already_done_is_swallowed_symmetrically` (tests.rs) pins this exact case (`real_err` → `.is_err()`).
- Dispatch alignment: confirmed — in `apply` (client.rs:383-449) `swallow` is computed once and passed only to the four idempotent ops; the non-idempotent ops (`chat.postMessage`, `chat.delete`, `chat.update`) hard-code `false`. The swallow class is exactly the already-satisfied class, never "all remove errors".

The widening is the minimal-blast-radius correct fix: it changes one boolean for four variants and relies on the unchanged code-recognizer to keep the class tight.

## Fix 2 — Pushdown residual (the t20 class): CORRECT, no silent-wrong-rows path

`Lowered::{Exact, PreFilter}` (pushdown.rs:91-100) cleanly discriminates the two cases:

- `Exact` (Ge/Le): inclusive SQL bound ≡ Slack inclusive `oldest`/`latest` → the param is the whole truth for that conjunct → residual **dropped** (`None`, pushdown.rs:71-74). Sound: the pushed set equals the predicate set.
- `PreFilter` (Gt/Lt): strict SQL bound lowered to Slack's inclusive bound is a **strict superset** (it also returns the `ts == X` boundary row) → param pushed AND the original strict predicate **kept** as residual (`Some(p.clone())`, pushdown.rs:79-82) → engine re-excludes the boundary row locally.
- Everything else (Eq/Ne, non-`ts` col, non-Text/Int literal) → `lower_cmp` returns `None` → kept residual (pushdown.rs:83). OR/NOT/IN/BETWEEN/LIKE stay wholly residual (pushdown.rs:87).

I checked specifically for the failure mode named in the brief — a path that **drops** a predicate while the pushed bound is a strict superset. There is exactly one drop path (`Exact`), and it is provably inclusive≡inclusive, so it is never a strict superset. The strict-superset case has its own arm (`PreFilter`) that retains the residual. No silent-wrong-rows path remains. The new test `strict_ts_gt_keeps_residual_and_re_excludes_the_boundary_row` (tests.rs) verifies both the retained residual and the actual boundary-row exclusion via a standalone ts evaluator (`{100,101}`→`{101}`, `{199,200}`→`{199}`), so the invariant is checked end-to-end, not just by shape.

## Fix 3 — Folded carry-overs: CORRECT

(a) `events` feature doc (Cargo.toml:30-37): now states plainly that `events` is a no-op *enabler* and the real dep-closure fence is the **absence of `runtime`** (`qfs-runtime` is `optional`), so the load-bearing wasm invocation is `--no-default-features [--features events]`. This is the truthful structural statement; the doc no longer implies `events` itself excludes the bridge.

(b) `SlackInbound::Unhandled` (events.rs:142-147, 235-238): an unknown signature-valid envelope is now a distinct `Unhandled { envelope_type, raw }` rather than a fabricated `Event` built from an envelope with no inner `event` shape. Verified:

- Pure / wasm-safe: it carries only `String` + `serde_json::Value`; it is produced inside `parse_event`, which is documented pure no-I/O (events.rs:188) and is the wasm subset's entry point. `SlackInbound` derives `Debug/Clone/PartialEq` (events.rs:126), so downstream logging/`{:?}` compiles.
- Acked, not dropped: `verify_signature(...)?` runs first (events.rs:204), so `Unhandled` is reached only for signature-valid envelopes, and it is returned as `Ok(...)` — a structured success the ingress can ack with 200 and log. An unknown envelope is therefore never silently dropped; it is surfaced as a fact and (correctly) **not** handed to the trigger bus as a synthetic event.

## Observations (per Critical Review Policy — ≥1, each with a proposal)

1. **`Unhandled` is a fact with no consumer in-tree.** The structurally-correct enum exists, but the actual ack-with-200-and-log behavior lives in the (future) Workers ingress, which is out of t25's tree. There is no compile-time guard that the ingress will branch on `Unhandled` rather than fall into a catch-all that drops it.
   - Proposal (carry-over, non-blocking): when the Workers WEBHOOK ingress lands (RFD §8), add an ingress-level test that an `Unhandled` delivery produces a 200 ack and a log line — closing the loop the driver can only assert structurally today. Track as an E3 carry-over, not a t25 blocker.

2. **`PreFilter` keeps the residual via `p.clone()` (the whole `Cmp`), which is the strict predicate itself — correct, but the residual now carries a conjunct that the pushed param *also* partially enforces.** This is sound (over-fetch then filter) and matches the t20 lesson, but it means the engine always re-evaluates the strict comparison even though the pushed bound already removed all rows below/above it; only the single `==X` boundary row is actually at stake.
   - Proposal: purely an efficiency note, not a correctness one — no change required for t25. If a future optimizer wants the tightest residual, it could narrow the kept residual to `ts != X` (the only row the inclusive bound over-returns) rather than the full strict `>`/`<`. Documenting this as an optional E3 refinement keeps the current truthful-but-broad residual as the safe default.

3. **Reviewer-noted, out of my domain:** the fix commit message itself flags that `tests/planner_e2e.rs` fails `clippy -D warnings` (missing the conventional test-allow header) and leaves it for the Planner. That is a Planner-owned file and a lint, not a structural defect in the fix — I note it only so the Lead routes it; it does not affect this approval.

## Cross-cut coherence

The three fixes are independent and each lands in the right layer: the swallow widening is a one-line selector change backed by an unchanged recognizer (tight blast radius); the pushdown discrimination is a type-level distinction that makes the soundness obligation explicit in the type system (`Exact` vs `PreFilter`); the events change is a new enum arm that preserves the signature-first ordering. No fix introduces I/O, token handling, or a wasm-unsafe dependency. Both defects the Planner found are closed at the structural level, and the closures are pinned by tests that check behavior (boundary-row exclusion; terminal-on-real-error), not just shape.

## Decision

**Approve with observations.** Both Planner-found defects are fixed correctly and minimally; the two folded carry-overs are structurally sound. The observations are carry-overs/efficiency notes and a routing pointer, none blocking.
