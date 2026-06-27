---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: []
---

# t62 — Reversible-only `TRANSACTION` + commit-point ordering

## Overview
Delivers the transactional half of **M6 — Language core** (decision G; roadmap §1.2): a
`TRANSACTION { … }` block that may contain **only reversible operations**. An irreversible effect
inside a transaction is a **parse/eval-time error** — rejected the same way an unsupported verb is
rejected today, *before* anything touches the world — and the reversible work inside commits
all-or-nothing via the existing saga/ACID machinery and commit-point ordering. `TRANSACTION` is the
**second and last** genuine new keyword the roadmap permits (with `LET`, t60); it MUST be added to
the frozen keyword fixture **deliberately** and the freeze tests updated as an intended vocabulary
change. The irreversibility classification (`EffectNode.irreversible`, `Plan::is_irreversible()`,
`is_inherently_irreversible()`) and the commit strategy/executor (`select_strategy`, `SagaExecutor`)
already exist as a library — this ticket adds the block, the parse/eval-time guard, and the
all-or-nothing commit wiring on top. Independent of `LET` (t60) — can land in parallel.

## Exact seams
- `crates/lang/src/keywords.rs` — `Keyword` enum + `KEYWORDS` slice (**frozen at 38**) + `from_word`.
  Add `Keyword::Transaction`, the `"TRANSACTION"` slice entry, the `from_word` arm, and deliberately
  update `keyword_enum_matches_golden_fixture`, `keyword_count_is_frozen` (38 → 39, or → 40 if t60
  also lands), and the `ALL_KEYWORDS` fixture. `OPERATORS` (15) untouched. (If t60 ships first the
  count is bumped from its value — coordinate the freeze edit; the tickets are otherwise independent.)
- `crates/parser/src/ast.rs` — `Statement` enum (Query/Effect/Ddl/Plan; **NOT** `#[non_exhaustive]`
  so the variant set is governance-locked). Add a `Statement::Transaction { body: Vec<Statement> }`
  (or `Vec<EffectStmt>`) variant; update the `Statement` variant-set governance test deliberately.
  The body holds effect statements (`EffectStmt`); reuse the existing `EffectVerb`/`EffectStmt`
  shapes — `TRANSACTION` adds no new effect kind.
- `crates/parser/src/grammar.rs` — winnow `parse_statement()` (ADR-0001 locked): add the
  `TRANSACTION { <stmt>; <stmt>; … }` block production with brace delimiters; parser golden tests.
- `crates/plan/src/node.rs` — `EffectKind` (Read/List/Insert/Upsert/Update/Remove/Call/
  ServerConfigWrite, `#[non_exhaustive]`), `is_inherently_irreversible()` (Remove = true; Call
  per-proc), `EffectNode` (the `irreversible` flag). These are the **classification authority** the
  guard reads — `TRANSACTION` adds NO new effect kind; it only *inspects* this flag.
- `crates/plan/src/plan.rs` — `Plan { nodes, edges }`, `Plan::is_irreversible()`, `topo_order`. The
  guard rejects a transaction whose plan `is_irreversible()`; `topo_order` provides the commit-point
  ordering for the all-or-nothing apply.
- `crates/core/src/eval.rs` — `Evaluator`, `eval_statement() -> EvalValue` (`Plan(Plan)`), `EvalError`.
  Evaluate the `TRANSACTION` body into one `Plan`, then assert reversibility: add
  `EvalError::IrreversibleInTransaction { … }` (per the spec) and raise it when any node is
  inherently irreversible (`is_inherently_irreversible()`) or flagged `EffectNode.irreversible` /
  `Plan::is_irreversible()`. This is the parse/eval-time gate — no I/O, fully dry-runnable.
- `crates/core/src/security.rs` — `IrreversibleGuard::require_ack(plan, mode, ack)`, `RunMode`,
  `NeedsPreview`. The transaction guard runs *earlier and stricter*: inside a `TRANSACTION` an
  irreversible op is never merely "needs an extra ack" — it is a hard error. Keep the existing guard
  for the outside-transaction case (the roadmap example sends the receipt OUTSIDE the block).
- `crates/txn/` — `select_strategy -> CommitStrategy { Acid, Saga }`, `SagaExecutor::run_acid` /
  `run_saga`, `EffectLeg`/`LegApplier`. A validated (all-reversible) transaction plan commits
  all-or-nothing: single transactional source → `run_acid`; cross-source reversible work →
  `run_saga` with reverse-order compensation. The block's commit point is the boundary after which
  any irreversible follow-up (the `CALL mail.send` in roadmap §1.2) runs separately.

## Implementation steps
1. **Keyword (deliberate freeze edit).** Add `Keyword::Transaction`, the `"TRANSACTION"` slice entry,
   and the `from_word` arm in `crates/lang/src/keywords.rs`; update `keyword_count_is_frozen` and the
   golden fixture / `ALL_KEYWORDS`, with a comment marking the intended M6 vocabulary change
   (decision G). `cargo test -p qfs-lang`; `cargo fmt`.
2. **AST + governance + grammar.** Add `Statement::Transaction { body }` to
   `crates/parser/src/ast.rs` (update the variant-set governance test), and the
   `TRANSACTION { … }` block production to `crates/parser/src/grammar.rs`. Parser golden tests for the
   roadmap §1.2 two-`UPSERT` block. `cargo test -p qfs-parser`.
3. **Reversible-only guard (the core of the ticket).** In `crates/core/src/eval.rs`, lower the block
   to one `Plan`, then walk its `EffectNode`s and reject via the new
   `EvalError::IrreversibleInTransaction` if `Plan::is_irreversible()` /
   `is_inherently_irreversible()` / `EffectNode.irreversible` is set. Unit tests: a block of two
   `UPSERT`s passes; a block containing `REMOVE` or `CALL mail.send` fails at eval time with the
   structured error and **zero** effects applied (purity proven).
4. **All-or-nothing commit.** Route a validated transaction plan through `crates/txn`
   `select_strategy` → `SagaExecutor::run_acid` (single transactional source) or `run_saga`
   (cross-source, reverse-order compensation), honoring `crates/plan` `topo_order` commit-point
   ordering. Tests with an in-memory fake applier: injected mid-commit failure leaves zero applied
   (ACID) or fully-compensated (saga). No live credentials.
5. **Gate + docs.** Ensure every exhaustive `match` on `Statement`/`EffectKind` handles the new arms
   (compiler-enforced). Run `cargo build/test/clippy --workspace`, `cargo fmt --all --check`,
   `cargo run -p xtask -- gen-docs --check` (regenerate `docs/language.md` to include `TRANSACTION` —
   never hand-edit). Bump the patch in `crates/qfs/Cargo.toml`.

## Key files
- `crates/lang/src/keywords.rs` — modify (`Keyword`, `KEYWORDS`, `from_word`, freeze tests).
- `crates/parser/src/ast.rs` — modify (`Statement::Transaction`, governance test).
- `crates/parser/src/grammar.rs` — modify (`TRANSACTION { … }` block production).
- `crates/core/src/eval.rs` — modify (lower block → `Plan`, `EvalError::IrreversibleInTransaction`).
- `crates/plan/src/node.rs`, `crates/plan/src/plan.rs` — read-only consumers of
  `is_inherently_irreversible()` / `Plan::is_irreversible()` / `topo_order` (no schema change).
- `crates/txn/src/{strategy,saga}.rs` — wire `select_strategy` / `SagaExecutor` for the block commit.
- Generated `docs/language.md` — regenerated via `cargo run -p xtask -- gen-docs` (never hand-edit).

## Considerations
- **The safety floor is *raised* here, not just preserved.** Outside a transaction, an irreversible
  effect needs an extra acknowledgement (`IrreversibleGuard::require_ack`). Inside a `TRANSACTION` it
  is a **hard parse/eval-time error** — the strongest possible posture (decision G). The roadmap
  example is the canonical shape: the two reversible `UPSERT`s live inside the block; the irreversible
  `CALL mail.send` lives OUTSIDE, after the commit point, with its own explicit ack. The guard must
  fire on `is_inherently_irreversible()` (Remove always; Call per-proc) AND the per-node
  `EffectNode.irreversible` flag, so a driver that marks an op irreversible at runtime is also caught.
- **Governance — deliberate keyword edit.** `TRANSACTION` is a *genuine* new keyword and must pass
  the `crates/lang/src/keywords.rs` freeze tests and the `crates/parser` `Statement` variant-set
  governance test; the PR must call this out as an intended closed-core vocabulary change. It adds NO
  new effect kind, NO new `PipeOp` — it is a statement wrapper over existing `EffectStmt`s. If t60
  (`LET`) lands in the same window, coordinate the single `keyword_count_is_frozen` value.
- **Dep-direction (`crates/cmd/tests/dep_direction.rs`).** Work is in pure cores
  (lang/parser/plan/core/txn); no new crate, no driver/runtime edge, tokio stays out, cores stay
  wasm-buildable. The commit wiring uses the existing `crates/txn` seam — no new dependency edge.
- **No distributed 2PC (RFD §6).** "All-or-nothing" for a single transactional source is real ACID;
  cross-source is the best-effort saga with reverse-order compensation and ledger recovery — that is
  why the block is *reversible-only*, so every leg has a compensation. Document this honestly: a
  cross-source `TRANSACTION` is saga-backed, not 2PC.
- **Open product decision to flag.** Brace-delimited `TRANSACTION { … }` vs a `BEGIN/COMMIT`-style
  form — prefer the brace block shown in roadmap §1.2 (no extra keywords). Also flag: whether nested
  transactions or a transaction containing a `LET` (t60) are allowed — keep conservative (no nesting)
  this slice so a later relaxation is non-breaking.
- **Docs honesty + versioning.** Do not advertise `TRANSACTION` in `docs/guide/*` or the skill until
  the slice ships and `gen-docs --check` is green (roadmap tags it 🧭). Own PR + patch bump in
  `crates/qfs/Cargo.toml` (currently 0.0.7) + a `v0.0.x` tag on ship.
