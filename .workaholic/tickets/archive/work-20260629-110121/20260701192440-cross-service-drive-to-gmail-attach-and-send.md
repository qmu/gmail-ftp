---
created_at: 2026-07-01T19:24:40+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash: 2a380d4
category: Added
depends_on: [20260701192439-query-array-struct-bytes-literals-gmail-draft-attachments.md]
---

# Cross-service Drive→Gmail attach-and-send: the composable ARRAY_AGG(STRUCT) pipe

## Overview

**Sub-ticket of EPIC `.workaholic/tickets/todo/a-qmu-jp/20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`.** The **dogfooding payoff**: *download a file from Google Drive, attach it to a Gmail draft, and send it* in one composable qfs statement.

The foundation ticket `20260701192439` **has landed** (archived `596a3ac`): `[ ]`/`{ }`/`X'..'` literal constructors, `Value::Array/Struct/Bytes` lowering, and the Gmail draft `attachments` `Array(Struct{filename,mime,bytes})` column all work end-to-end for **inline literals**. `20260701192441` (attachment byte-read) and `20260701192442` (mkdir parity) also landed.

**The owner's chosen shape (confirmed 2026-07-01).** Per the *pipe-syntax-in-SQL* model (fully composable `|>` operators), the recipe is the standard `ARRAY_AGG(STRUCT(...))` composition — NOT a bespoke `pack()` special form:

```qfs
/drive/my/report.pdf
|> select {filename: name, mime: mime_type, bytes: content} as att   -- per-row Struct from columns
|> aggregate array_agg(att) as attachments                            -- N rows → one Array(Struct)
|> extend to = 'a@x.y', subject = 'Q3', body = 'See attached'
|> insert into /mail/drafts
```

Two small, reusable, composable primitives — a **struct-over-expressions constructor** and a **single-column `ARRAY_AGG`** — instead of one monolithic multi-column aggregate. `ARRAY_AGG` is single-column, so it fits the existing single-column `Aggregate` struct with **no invasion of the closed aggregate representation** (this was the key reason to prefer it over a multi-column `pack`).

## ⚠️ The real blocker discovered during scoping: no general per-row scalar-expression executor on the read path

This is **bigger than "add an aggregate."** qfs's read execution today runs only:

- **`WHERE`** as a **lowered `Predicate`** (`col op lit`) via `engine::eval_predicate` (`crates/engine/src/eval.rs:22`) — NOT a general `Expr`.
- **`SELECT`/`EXTEND`** projections that are **by-name only**: `engine::project(batch, columns: &[Name])` (`crates/engine/src/eval.rs:137`) selects/renames columns; `core::eval::project_schema` (`crates/core/src/eval.rs:~867`) *types* `fn(...)`/other projections but their **values are late-bound / not executed** on the read path.
- **`AGGREGATE`** via `pushdown::Aggregator` (a **closed** enum `Count/Sum/Min/Max`, `crates/pushdown/src/logical.rs:~73`), executed single-column by `engine::run_aggregate` (`crates/engine/src/eval.rs:320`). `aggregator()` in `crates/pushdown/src/lower.rs:~336` rejects unknown names (note: even `AVG` is registered in `stdlib` but NOT here).

There is **no** engine step that evaluates an arbitrary `Expr` (a struct constructor, an array constructor, a scalar `fn`) to a `Value` per row inside a projection or an aggregate argument. So `select {filename: name} as att` and `array_agg(att)` cannot run until that executor exists. **The bulk of this ticket is building that per-row scalar-expression execution**, then `ARRAY_AGG` rides on top.

## Implementation plan (ordered)

### 1. Generalize struct/array construction from literal to expression (evolves 192439)
- Today: `Literal::Array(Vec<Literal>)`, `Literal::Struct(Vec<(String,Literal)>)` in `crates/parser/src/ast.rs` (literal-valued only). `Literal::Bytes` stays a scalar literal.
- Change: introduce `Expr::Array(Vec<Expr>)` and `Expr::Struct(Vec<(String,Expr)>)` (values are full exprs). Parse `[ ... ]`/`{ ... }` into these **Expr** forms in `crates/parser/src/grammar.rs` `primary()` (currently `array_literal`/`struct_literal` produce `Literal`). Retire `Literal::Array/Struct` (experimental → no compat shim). Keep the all-literal case working: `values_row_batch`/`literal_value` (`crates/core/src/eval.rs:~777`, `~1074`) must evaluate a constant `Expr::Array/Struct` to a `Value` (this is how the 192439 inline-attachment cookbook recipe keeps parsing/executing).
- Update every match that referenced `Literal::Array/Struct`: `core/src/eval.rs` `literal_to_value`, `core/src/lambda.rs` `literal_to_value`, `core/src/typeck.rs` `literal_type`, `crates/server/src/lower.rs` composite-literal rejection, and the parser test `insert_draft_with_array_struct_bytes_attachment_literal` (assert `Expr::Array` now).

### 2. Build the per-row scalar-expression executor (the real work)
- Add a general `eval_value(expr: &Expr, schema, row) -> Result<Value, _>` in the engine (extend `crates/engine/src/eval.rs`; `core/src/lambda.rs:144 eval_expr` is the closest existing shape — it already handles `Expr::Lit/Col/Fn` for lambda bodies and could be the seam to reuse or mirror). It must handle `Expr::Col` (positional lookup, incl. `.`-navigation like `resolve` at `engine/src/eval.rs:48`), `Expr::Lit`, `Expr::Struct` (build `Value::Struct(Fields)` from evaluated field exprs — field names = the keys), `Expr::Array` (build `Value::Array`), and `Expr::Fn` (scalar fns via the stdlib registry).
- Wire it into `SELECT`/`EXTEND` execution so a projection/assignment expression produces a real column value (today the physical projection is name-only). This is the load-bearing addition; keep the `PhysicalPlan` change minimal and covered by the engine's naive-eval property (ADR-0002, `crates/engine/src/lib.rs`).

### 3. Add the single-column `ARRAY_AGG` aggregate
- `crates/pushdown/src/logical.rs`: add `Aggregator::ArrayAgg`.
- `crates/pushdown/src/lower.rs` `aggregator()` (~336): map `"ARRAY_AGG"` → `Aggregator::ArrayAgg` (arg is the struct-expr column produced by step 2, or evaluate the arg expr per row before collecting).
- `crates/engine/src/eval.rs` `run_aggregate` (320): `ArrayAgg` arm collects the column's per-row values (in row order) into `Value::Array`. Output column type `Array(elem)` (line ~291 sets the agg column type).
- `crates/core/src/typeck.rs`: type `ARRAY_AGG(x)` as `Array(<x's type>)`; register it wherever `COUNT/SUM/...` return types resolve (`stdlib/aggregate.rs` `aggregate_builtins` + the `check_fn`/aggregate path ~236-282) so it isn't rejected as an unknown aggregate. Reconcile the two aggregate representations (`stdlib::AggregateKind` for typeck vs `pushdown::Aggregator` for execution).

### 4. INSERT ... FROM folding + column shaping
- `crates/core/src/eval.rs` `effect_input_schema` (~823) already folds a `FROM <pipeline>` body's output schema for the effect; confirm the packed `attachments` column + the `EXTEND`ed `to`/`subject`/`body` land as the draft's named columns (the applier reads by name — `driver-gmail/src/effect.rs` `draft_from_row`/`attachments_col`).
- Column rename is `SELECT expr AS alias` (already the projection-alias mechanism); the `content`→`bytes`, `name`→`filename`, `mime_type`→`mime` mapping happens inside the struct literal keys (`{filename: name, ...}`), so no separate rename step is needed once step 2 works.

## Key Files
- `crates/parser/src/{ast.rs,grammar.rs,tests.rs}` — `Expr::Array/Struct`; parse `[ ]`/`{ }` as exprs.
- `crates/engine/src/eval.rs` — the new per-row `eval_value`; `run_aggregate` `ArrayAgg`; projection execution of expressions.
- `crates/pushdown/src/{logical.rs,lower.rs}` — `Aggregator::ArrayAgg`.
- `crates/core/src/{eval.rs,typeck.rs,lambda.rs}` — constant `Expr::Array/Struct` folding, typing, `ARRAY_AGG` return type; drop `Literal::Array/Struct`.
- `crates/server/src/lower.rs` — composite-literal rejection now only `Literal::Bytes`.
- `crates/driver-gdrive/src/read.rs` `content_batch` (source `content(Bytes)`); `crates/driver-gmail/src/effect.rs` `attachments_col` (sink).
- `docs/cookbook/cross-service.md` (+ `gmail.md`) recipe; `docs/guide/replace-gmail-gdrive-ftp.md` cross-service row.

## Policies
- `workaholic:implementation` / `type-driven-design.md` — `Expr::Struct/Array` lower to rich `Value::Struct/Array`; `ARRAY_AGG` return type is `Array(elem)`; shape errors are `Result`, never panics.
- `workaholic:implementation` / `domain-layer-separation.md` — the scalar executor + `ARRAY_AGG` live in the engine/core (closed core), general (not Gmail/Drive-specific); one driver never imports another.
- `workaholic:planning` / `ai-native-future.md` — expressible in the one grammar + describe→preview→commit; send stays behind the explicit irreversible `CALL mail.send` + `--commit-irreversible`.
- **Anti-drift (CLAUDE.md):** cross-service recipe in a cookbook article (parse-checked by `cookbook_skills.rs`); regenerate SKILL.md via `gen-skills`; any new syntax shown must actually parse.

## Quality Gate
**Acceptance:**
- `/drive/... |> select {filename: name, mime: mime_type, bytes: content} as att |> aggregate array_agg(att) as attachments |> extend to/subject/body |> insert into /mail/drafts` parses, type-checks, and yields a draft whose `attachments` carries the Drive file's bytes/filename/mime.
- The struct-over-expressions constructor and `ARRAY_AGG` are **general** (usable beyond this recipe), matching the pipe-syntax composability goal.
- Cross-service recipe passes the `cookbook_skills.rs` parse ratchet.
- Patch version bumped on the shipped PR.

**Verification** (from `packages/qfs`, `TMPDIR` redirected off the tmpfs, `command rm`, `source ~/.cargo/env`):
- `cargo build --workspace`, `cargo test -p qfs-test`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check` green.
- `cargo run -p xtask -- gen-docs --check` and `gen-skills --check` green.
- **Live proof:** against the owner's real Google account, run the documented recipe to download an actual Drive file, attach it, and send; confirm the received Gmail carries the correct file.

## Considerations
- **Scope:** this is a language-feature ticket (per-row scalar-expression execution in projections/aggregates), not a small aggregate add — realistically **1–2 days**, well beyond the 4h effort bucket. Do it in a focused session.
- Building the scalar executor is the risky part (touches the closed-core physical execution); land it behind the engine's naive-eval property tests first, then add `ARRAY_AGG`, then the recipe.
- A large Drive file becomes a `Value::Bytes` cell flowing through the effect pipeline — avoid unnecessary copies; note any practical size ceiling in the cookbook.
- Retiring `Literal::Array/Struct` in favour of `Expr::Array/Struct` is an experimental hard-break (no compat shim, per memory) — update the 192439 parser test accordingly.

## Final Report

Delivered the **read-path composable `ARRAY_AGG(STRUCT)` pipe** — the "real blocker" this ticket
identified as the bulk of the work — end to end, fully green across every gate. Steps 1–3 of the plan
are complete; step 4's *language* piece (the packed columns land by name) is proven, but the terminal
`INSERT … FROM` **commit-side materialisation** and the **live send** are NOT done (see below).

### What landed
- **Step 1 — `Expr::Array`/`Expr::Struct` (generalised from literals).** Retired `Literal::Array/Struct`
  (experimental hard-break); `[ … ]`/`{ … }` now parse as expression constructors whose elements are
  full sub-expressions. Inline-literal cases (192439's attachment recipe) preserved via constant-folding
  in `literal_value`. All Expr-walking passes updated (core eval/lambda/typeck/resolve/ddl-spec, server
  lower, http rewrite, watchtower bind).
- **Step 2 — per-row scalar executor.** New `qfs_pushdown::ScalarExpr` (`Col`/`Lit(Value)`/`Array`/
  `Struct`), lowered from the AST in `pushdown::lower` (a scalar `fn(...)` in a projection stays a
  structured `LowerError` — it needs the stdlib the engine doesn't depend on, an intentional acyclic
  boundary). New residual ops `LogicalPlan::ProjectExpr`/`Extend` → `CombineOp::ProjectExpr`/`Extend`
  (always local; the pushable name-only `Project` path is untouched, so no pushdown/explain golden
  drift). `engine::eval_value`/`project_expr`/`extend` evaluate them per row. **EXTEND now actually
  computes values on the read path — it was a silent schema-only pass-through before.**
- **Step 3 — single-column `ARRAY_AGG`.** `Aggregator::ArrayAgg` (a faithful collect, keeps nulls, row
  order) across `pushdown` + `engine::run_aggregate` + `stdlib::AggregateKind` + the typeck return-type
  path (`Array(elem)`). Fits the closed single-column `Aggregate` shape — no invasion of the aggregate
  representation.

### Verified
- Hermetic differential test `cross_service_pack_attachments_project_expr_array_agg_extend`
  (`crates/engine/tests/engine.rs`): ProjectExpr → ARRAY_AGG → Extend over an in-memory Drive scan
  yields one row whose `attachments` is an `Array` of two `Struct`s carrying each file's bytes/filename/
  mime, plus the extended draft columns.
- The **real binary** runs the composable pipe end to end (read/preview): `… |> select {filename: name,
  mime: mime_type, bytes: content} as att |> aggregate array_agg(att) as attachments |> extend to=…,
  subject=…, body=…` → `{"attachments":[{…},…],"to":…,"subject":…,"body":…}`.
- All gates green: `build`, full `test` suite, `clippy -D warnings`, `fmt --check`, `gen-docs --check`,
  `gen-skills --check`, and the `cookbook_skills` parse ratchet (the new cross-service recipe parses).
  Version stays `0.0.12` (one bump for the whole PR, per the carry ticket).

### Remaining (follow-up — genuinely out of this read-path feature's reach)
- **`INSERT … FROM <pipeline>` commit-side materialisation.** The effect plan's `Read` node
  (`core/eval.rs` `eval_write`) is only a *dependency marker* carrying the source target; the write's
  `args` stay empty for a pipeline body, and the runtime interpreter never re-executes the transform to
  feed the applier. So the terminal `|> insert into /mail/drafts` does not yet write the *packed* rows at
  commit. This is a **pre-existing, separate runtime/interpreter gap** (step 4 of the plan assumed it
  already worked). File a follow-up: wire the interpreter to run the effect's `FROM` pipeline via the
  read executor and hand its `RowBatch` to the write applier.
- **Live send acceptance.** Requires the owner's real Google account + the irreversible gate
  (`CALL mail.send` / `--commit-irreversible`); not autonomously verifiable.

### Discovered Insights
- **Insight**: Reads never touch the pure evaluator's `PlanSource` — `execute_read` lowers the AST
  **directly** via `plan_query` → `pushdown::lower_query` → `partition_by_source` → `PhysicalPlan`, then
  `MiniEvaluator`. `PlanSource` (`core/eval.rs`) is schema-only, for preview/typing.
  **Context**: any new *runtime* read behaviour must be threaded through the pushdown `LogicalPlan` →
  `PhysicalPlan`/`CombineOp` layers and the engine kernels, not `PlanSource`.
- **Insight**: There are two aggregate representations — `stdlib::AggregateKind` (typeck + the
  `AggregateState` machine, used only by stdlib tests today) and `pushdown::Aggregator` (the actual read
  execution via `engine::run_aggregate`). `AVG` exists only in the former.
  **Context**: a new aggregate must be added to **both** to type-check *and* execute; keep their
  semantics aligned (e.g. `ARRAY_AGG`'s null handling).
- **Insight**: `EXTEND`/`SET` on the read path produced no runtime column before this ticket (dropped in
  both lowerings). Anything relying on a computed `EXTEND` column downstream was silently getting nothing.
  **Context**: `EXTEND` is now a real residual op; watch for pipelines that assumed the old no-op.
