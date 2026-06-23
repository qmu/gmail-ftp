# Planner E2E Review — t25 Slack driver

- **Reviewer**: Planner (Progressive / business outcome + E2E external-interface QA)
- **Ticket**: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t25-driver-slack.md`
- **Under test**: Constructor commit `c5cb1ab` (implementation), Architect approval `736b6c7`
- **Method**: **black-box E2E** — independent Planner-owned harness driving the crate's **public API** + the **runtime interpreter** (true `COMMIT` path) + a **Planner-owned scripted HTTP transport** (so the real `RestSlackClient` seam — Bearer injection, cursor pagination, retry, BodyErrorRule — runs over the wire) + the pure `parse_event` normalizer from event fixtures. The event-signing oracle is an **independent, dependency-free HMAC-SHA256/SHA-256** written in the harness (not the driver's internal `hmac` module), so the verify side is checked against a separate implementation. **No live Slack, no live token, no network.**
- **Harness**: `crates/driver-slack/tests/planner_e2e.rs` (28 tests). Not a deliverable replacement for review — an external-interface probe of the acceptance criteria.

## Decision

**Request revision (Planner)** — 26/28 E2E checks pass; **2 fail and they are a real, reproduced defect**, not a harness artifact. The defect is exactly the regression the Architect flagged: the already-done swallow-set covers the **add** side but **not the remove side**, so idempotent REMOVE/unpin surface as **terminal errors** instead of swallowed no-ops. This breaks the RFD §6 idempotency contract the driver's own docs state. One additional truthfulness observation on strict `>` pushdown (lower severity). Everything else (DESCRIBE, plan shapes, capability gating, event signature + replay defense, pagination + cap, BodyErrorRule, token + signing-secret safety) is validated green.

## Results per scenario (ran / expected / actual)

| # | Scenario | Result |
|---|----------|--------|
| 1 | DESCRIBE per-node archetype + schema + capability set | PASS — messages/replies/reactions/dms=`append_log`, files=`blob_namespace`, users=`relational_table`; schemas + node-keyed cap sets correct; bare workspace root → structured `invalid_path` |
| 2 | Plan-shape goldens (post / reply thread_ts / react≡INSERT / pin irreversible / REMOVE→chat.delete) | PASS — `client_msg_id` attached (`cfs-…`); CALL react and INSERT reactions produce the identical `AddReaction`; pin is one `Call` node, `irreversible=true`, PREVIEW surfaces it; REMOVE→`DeleteMessage`, `is_irreversible()=true` |
| 3 | Capability gating at PARSE time (`INSERT`/`UPDATE` on users) | PASS — both rejected as structured `cfs_driver::CfsError::UnsupportedVerb` (`code=unsupported_verb`) listing `supported=["SELECT"]`; this is the parse-time gate, not an apply-time failure |
| 4 | Event tests (url_verification challenge / message+reaction normalize / tampered sig / wrong secret / stale ts both directions / inside-window / missing headers) | PASS — challenge echoed; `event_id` surfaced for dedupe; reaction reads channel/ts from `item`; tampered body, flipped sig, and wrong secret all → `bad_signature`; future- and past-skew both → `stale_timestamp`; just-inside-window accepts; missing sig/ts → `missing_header` |
| 5 | Pagination (3 cursor pages concatenated; page cap) | PASS — 3 pages merged in order through the real client (`cursor=C2`, `cursor=C3` observed); a 100-page runaway is bounded to ≤50 fetches (`MAX_PAGES`), one row per fetched page |
| 6 | BodyErrorRule (HTTP 200 + `ok:false` → terminal, read + write) | PASS — write path: `terminal`, carries `not_in_channel`, issued exactly once (no retry); read path: structured `slack_body_error` carrying `channel_not_found`; full-interpreter negative E2E records a `Failed` leg |
| **7** | **Remove-side idempotency (no_reaction / not_pinned)** | **FAIL — DEFECT CONFIRMED (see below)** |
| 8 | Pushdown residual truthfulness | PASS for the headline case; **one lower-severity observation** on strict `>` (see below) |
| 9 | Token + signing-secret safety (planted canaries) | PASS — see below |

## Finding #7 (DEFECT — the Architect's flag, reproduced end-to-end)

Driven through the **real `RestSlackClient` over the scripted transport** and observed via the runtime `SharedApplier` disposition (the COMMIT path):

- **`REMOVE` a reaction already absent** — mock returns HTTP 200 `{"ok":false,"error":"no_reaction"}`:
  - **Expected (idempotency contract):** swallowed no-op → `Ok`.
  - **Actual:** `SURFACED as terminal -> Slack API reactions.remove returned ok=false: no_reaction`.
- **`CALL slack.unpin` on an already-unpinned message** — mock returns HTTP 200 `{"ok":false,"error":"not_pinned"}`:
  - **Expected:** swallowed no-op → `Ok`.
  - **Actual:** `SURFACED as terminal -> Slack API pins.remove returned ok=false: not_pinned`.
- **Control (add side):** `already_reacted` on INSERT reaction and `already_pinned` on `slack.pin` **are** swallowed → `Ok`. So the swallow machinery works; it is simply not wired for the remove side.

**Root cause (precise):** `SlackEffect::swallows_already_done()` in `crates/driver-slack/src/effect.rs:275-280` matches **only** `AddReaction` and `Pin`:
```rust
matches!(self, SlackEffect::AddReaction { .. } | SlackEffect::Pin { .. })
```
It does **not** match `RemoveReaction` or `Unpin`. So `apply()` (client.rs:419-448) passes `swallow=false` for both `reactions.remove` and `pins.remove`, and `BodyErrorRule::check` raises a terminal `SlackError::Body`. Meanwhile `is_already_done()` (client.rs:214-219) **explicitly recognizes `no_reaction` and `not_pinned`** as the already-done class, and the doc comment there states the intended contract: *"the symmetric `no_reaction`/`not_pinned` (the remove already landed) are no-op successes."* The recognizer and the doc agree the remove side should be swallowed; the `swallows_already_done()` gate that selects it is the gap. The set is effectively dead code for the remove side.

**Business impact:** an idempotent "ensure this reaction/pin is gone" effect — exactly the UPSERT-shaped, replay-safe shape the ticket's Considerations require for at-least-once event-driven plans (RFD §6) — fails terminally on the second delivery. A trigger that fires "remove reaction on un-react" or "unpin on archive" will record a spurious terminal failure (and skip its dependents) whenever the desired end-state already holds. This directly undercuts the at-least-once event story the ticket sells.

**Proposed fix (one line, business-framed as "make remove idempotent like add"):** extend the match to the symmetric remove ops:
```rust
matches!(self,
    SlackEffect::AddReaction { .. } | SlackEffect::RemoveReaction { .. }
    | SlackEffect::Pin { .. } | SlackEffect::Unpin { .. })
```
This is consistent with the existing `is_already_done()` recognizer (no change needed there) and with the stated contract. A regression test should pin both `no_reaction` and `not_pinned` as `Ok` no-ops. **Severity: this is a Request-revision candidate; Lead decides.** (Constructor's own 39 unit tests pass — they exercise `already_reacted`/`already_pinned` on the add side only, so they never caught this. The control test in my harness proves the add side is fine.)

## Finding #8 (lower-severity truthfulness observation — strict `>` pushdown)

Headline case is correct: `ts >= 100 AND text = 'hi'` pushes `oldest=100` (exact for `>=`, since Slack `oldest` is inclusive) and **keeps `text = 'hi'` residual** — no wrong rows. `OR` stays wholly residual. Good.

**Probe (recorded, not a hard suite failure):** a **strict** `ts > 100` lowers to `oldest=100` **and drops the residual** (`params=[("oldest","100")] residual=None`). Slack's `oldest` is **inclusive**, so the row at exactly `ts==100` is over-returned **and not re-excluded locally** — an off-by-one truthfulness gap on the strict boundary. The `pushdown.rs` `lower_cmp` treats `Gt` and `Ge` identically (both → `oldest`) and unconditionally drops the conjunct. For `Ge`/`Le` this is exact; for `Gt`/`Lt` it is not.

**Proposed fix:** for a strict `Gt`/`Lt`, still push `oldest`/`latest` (over-fetch is fine) but **keep the strict comparison residual** so the engine re-excludes the boundary row — i.e. only drop the conjunct from the residual for the inclusive ops (`Ge`/`Le`). Low severity (Slack `ts` collisions at an exact boundary are rare and the over-return is a superset, never a missing row), but it is the same "truthful residual" invariant the t20 lesson is built on, so worth a follow-up. Lead's call whether to bundle with #7 or defer.

## Finding #9 (token + signing-secret safety — both canaries clean)

Planted **two** canaries — `xoxb-PLANNER-CANARY-bot-token-…` (bot token) and `PLANNER-CANARY-signing-secret-…` (signing secret) — and scanned every externally observable surface:

- Bot token: **absent** from the serialized plan and PREVIEW; the real client **does** inject `Authorization: Bearer <token>` (auth works) but the recorded `HttpRequest` `Debug` shows the `cfs_secrets::REDACTED` marker, **never** the token.
- **Signing secret (the Architect's second flag): absent** from `EventError` (on a bad-signature reject) and from the normalized `SlackInbound` `Debug` (on a good parse). The secret never reaches a structured error or a DTO.
- All four `SlackError` variants and the `SlackWsConfig` `Debug` are secret-free (config carries credential **keys** = selectors, not values).

No token or signing-secret material leaked on any plan / preview / error / Debug / log surface.

## Cross-artifact coherence

The driver faithfully realizes the ticket's RFD mapping: universal CRUD → Web API, multi-archetype per node, irreversible flags on pin/delete, pure planning with applier-side I/O, and the wasm-safe pure event subset. The one structural gap (#7) is a narrow contract omission in the swallow-set selector, not a design break — the recognizer, the docs, and the add side all already encode the right intent. Fixing #7 brings the implementation back in line with its own stated contract.

## Suite

`crates/driver-slack/tests/planner_e2e.rs` — 28 tests: **26 pass, 2 fail (Finding #7)**. Constructor's in-crate suite: 39 pass. The 2 failures are the deliverable signal of this review.

---

# Re-test gate (round 2) — Constructor fix `14a1d96`, Architect re-review `6d0c2d9`

- **Reviewer**: Planner — re-test of the two findings through the same black-box harness.
- **Decision: Approve — both fixes verified, re-test 30/30 green, workspace gates green.**

## #7 remove-side idempotency — FIXED, verified end-to-end

Driven through the real `RestSlackClient` over the scripted transport, observed via the COMMIT-path disposition:
- `REMOVE` an already-absent reaction (`{"ok":false,"error":"no_reaction"}`) → **swallowed as a no-op success (`Ok`)** — was terminal before. (`s7_remove_absent_reaction_no_reaction_outcome`)
- `slack.unpin` already-unpinned (`{"ok":false,"error":"not_pinned"}`) → **swallowed as a no-op success (`Ok`)** — was terminal before. (`s7_unpin_already_unpinned_not_pinned_outcome`)
- **Swallow is the already-satisfied class ONLY (new guard test):** a genuine `reactions.remove` failure (`message_not_found`) and a genuine `slack.unpin` failure (`channel_not_found`) **still surface as terminal**, carrying the real Slack code — the fix does not mask real failures. (`s7_genuine_remove_failure_still_surfaces_terminal`)
- Add-side control still swallows `already_reacted`/`already_pinned`.

Root cause confirmed closed: `SlackEffect::swallows_already_done()` (effect.rs) is now symmetric across the add/remove pair (`AddReaction | RemoveReaction | Pin | Unpin`), in sync with `is_already_done()`'s recognizer.

## #8 strict-ts residual — FIXED, verified

- **Strict `ts > 100`** now pushes `oldest=100` (over-fetch is safe) AND **keeps the strict `ts > 100` residual** so the engine re-excludes the `ts==100` boundary row Slack over-returns → no wrong rows. Symmetric `ts < 200` keeps its residual too. (`s8_strict_gt_pushes_oldest_but_keeps_strict_residual`)
- **Inclusive control:** `ts >= 100` / `ts <= 200` still correctly **drop** the residual (the inclusive Slack bound is exact) — the fix did not become over-conservative. (`s8_inclusive_ge_still_drops_residual`)
- The mixed-predicate headline case (`ts >= 100 AND text = 'hi'`) and the `OR`-stays-residual case remain green.

Driver change confirmed: `pushdown.rs` now distinguishes `Lowered::Exact` (`>=`/`<=`, drop residual) from `Lowered::PreFilter` (`>`/`<`, push param but keep the strict comparison residual) — exactly the truthful-residual fix proposed.

## Self-artifact lint fix (Planner QA domain)

`crates/driver-slack/tests/planner_e2e.rs` was missing the conventional test-allow header. Added `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` at the crate root (matching `crates/driver-github/tests/e2e_blackbox.rs`). `cargo clippy --workspace --all-targets -- -D warnings` is now **fully green** (previously ~46 lint errors from this file).

## Gates

- `cargo test -p cfs-driver-slack --test planner_e2e`: **30/30 pass** (was 26/28).
- `cargo clippy --workspace --all-targets -- -D warnings`: **green**.
- `cargo test --workspace`: **green, 730 passed, 0 failed** (Constructor reported 728; +2 are my new guard tests — genuine-remove-failure and the split strict/inclusive residual assertions).

**Verdict: re-test approved 30/30; the `--all-targets` clippy gate is green. No remaining concerns.**
