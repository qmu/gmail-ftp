---
created_at: 2026-06-26T10:32:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: M
commit_hash: 6d8497e
category: Changed
depends_on: []
---

# t72 — Write-form grammar: writes as pipeline stages (decision Q)

## Overview

Implements roadmap **decision Q**: a write reads as **dataflow**. When it has a source it is a
**pipeline stage** — `FROM <source> |> … |> INSERT/UPSERT/UPDATE/REMOVE <target>`; a **source-less
literal** write leads with the verb — `INSERT INTO <target> VALUES (…)`. Both forms are legal; nothing
else is. This is the form the roadmap prose and the query cookbook overwhelmingly use, but **today's
parser only accepts the verb-leading form** (`INSERT INTO <target> (VALUES … | <pipeline-as-source>)`)
— `pipe_op` has no insert/upsert/update/remove stage. This ticket adds the pipeline-stage write so the
implemented grammar matches the documented one (the divergence recorded in the project's grammar
notes), without removing the legal source-less `INSERT INTO … VALUES …` literal form.

## Exact seams

- `crates/parser/src/grammar.rs` `pipe_op` (line ~607) — today lists `where/select/extend/set/
  aggregate/group_by/order_by/limit/distinct/join/union/except/intersect/as/expand/decode/encode/
  **call**` but NO write verbs. Add `insert_into`/`upsert_into`/`update`/`remove` as **pipe stages**
  whose source is the upstream pipeline (the rows flowing in), producing the same `EffectStmt` the
  current `effect_stmt` builds — so the lowering/AST is shared, only the entry point is new.
- `crates/parser/src/grammar.rs` `effect_stmt`/`write_target`/`update_stmt`/`remove_stmt` (lines
  ~808–895) — keep the verb-leading statement form for the **source-less literal** case
  (`INSERT INTO <target> VALUES …`); factor the shared tail so a pipe-stage write and a statement-lead
  write build the same `EffectStmt { verb, target, body, returning }`.
- `crates/parser/src/ast.rs` — `Statement`/`PipeOp`/`EffectStmt`: a pipeline that ends in a write stage
  becomes an effect-producing query. Decide the AST shape (a write `PipeOp`, or a `Pipeline` whose
  terminal stage is an effect) and keep the governance exhaustiveness tests in `crates/core` honest
  (the `Statement`/`PipeOp` matches in `eval.rs`/`resolve.rs` must handle the new variant).
- `crates/core/src/eval.rs`/`resolve.rs` — the evaluator already turns an `EffectStmt` into an
  effect-plan; route the pipe-stage write through the same path so `FROM /a |> … |> UPSERT INTO /b`
  and `UPSERT INTO /b FROM /a |> …` produce the **same** `Plan`.
- Governance: `crates/lang/src/keywords.rs` — **no keyword change** (`INSERT INTO`/`UPSERT INTO`/
  `UPDATE`/`REMOVE`/`VALUES`/`RETURNING` are already frozen). The freeze tests stay green; this is a
  grammar *shape* change, not a vocabulary change.
- Anti-drift: the query-cookbook parse harness (`packages/qfs/crates/test/tests/roadmap_cookbook.rs`)
  — many `grammar=extended` recipes use `… |> INSERT INTO …`; once this lands they parse, so the
  retag flips them to `core` and `BASELINE_CORE` should be bumped to the new (higher) count.

## Implementation steps

1. **Shared effect tail.** Refactor `write_target`/`update_stmt`/`remove_stmt` so the `EffectStmt`
   builder is reusable from both a statement-leading and a pipe-stage entry. No behavior change yet;
   tree green, existing parse goldens unchanged.
2. **Pipe-stage writes.** Add `insert_into`/`upsert_into`/`update`/`remove` to `pipe_op`, taking the
   upstream pipeline as the source. Parse tests: `FROM /sql/pg/x |> WHERE … |> UPDATE SET …`,
   `FROM /a |> ENCODE csv |> UPSERT INTO /drive/r/o.csv`, `FROM /src |> INSERT INTO /mail/drafts` parse
   and lower to the same `EffectStmt` as their verb-leading equivalents.
3. **Keep the literal form.** `INSERT INTO /target VALUES (…)` (source-less) still parses as a leading
   statement. Add a parse test pinning both forms legal and a NEGATIVE test that a write stage with no
   upstream source (e.g. a bare `|> INSERT INTO` with nothing before it) is a clear error.
4. **Evaluator parity.** Assert (plan-shape golden) that the two spellings produce identical effect
   plans; route through the existing `eval`/`resolve` effect path. Update `crates/core` exhaustiveness
   matches for the new AST variant.
5. **Docs + corpus.** Re-run the cookbook retag; bump `BASELINE_CORE`. Regenerate `docs/{language,
   server}.md` (`cargo run -p xtask -- gen-docs --check`). The roadmap/cookbook prose already match
   decision Q; confirm no example now contradicts it.

## Key files

- `crates/parser/src/grammar.rs` (`pipe_op` + the refactored effect tail), `crates/parser/src/ast.rs`.
- `crates/core/src/{eval.rs,resolve.rs}` (route pipe-stage writes; exhaustiveness).
- `crates/test/src/parse_golden.rs` + goldens; `packages/qfs/crates/test/tests/roadmap_cookbook.rs`
  (`BASELINE_CORE` bump after retag).
- Generated `docs/language.md`/`docs/server.md`; `crates/qfs/Cargo.toml` (patch bump).

## Considerations

- **Two legal forms, one plan.** The whole point is that `FROM … |> INSERT INTO …` and the source-less
  `INSERT INTO … VALUES …` are the same effect rendered two ways; they MUST lower to identical plans,
  proven by a golden. Anything that is neither (a write stage with no source, a third spelling) is an
  error, not a silent acceptance.
- **No vocabulary change.** Closed-core keywords are untouched; the freeze tests are the tripwire that
  this stayed a grammar-shape change. Pairs naturally with the M6 language batch (t60/t61) but does
  not depend on them.
- **Safety floor unchanged.** A pipe-stage write still previews/commits through the same gate; an
  irreversible write (`REMOVE`, `CALL` after) still needs `--commit-irreversible`, and a reversible-
  only `TRANSACTION` still rejects an irreversible stage at parse time (decision G).
- **Ordering.** Land before the big cookbook `=`→`==` migration ([[t70]]) so the two grammar passes
  over the example corpus can be coordinated (both touch the same recipes).
- **Versioning:** own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
