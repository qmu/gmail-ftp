---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: da4c2b3
category: Added
depends_on: [20260622214650-t04-grammar-ast-and-governance.md, 20260622214650-t05-type-schema-model.md, 20260622214650-t09-effect-plan-and-preview-commit.md]
---

# Evaluator → effect-plan (pure evaluation)

## Overview

This ticket delivers the **pure evaluator**: the function that turns a typed AST
statement (#4) into a **value** — a relation/plan-source for the query side and an
**effect-plan node** for the write/`CALL` side — performing **no I/O**.

It implements RFD §3 (purity invariant), §2.2–2.3 (pipe-SQL query side is pure;
write operators evaluate to a `Plan`), and §6 (effect-plan as a typed DAG). The
evaluator is the heart of "a statement is a plan": query stages fold to a logical
relation/plan-source description, write operators (`INSERT/UPSERT/UPDATE/REMOVE`)
and `CALL driver.action(...)` **construct** effect nodes, and aliases like `SEND`
desugar to a `CALL` per their pure `… -> Plan` typing. The single impure boundary —
`COMMIT : Plan -> World -> World` — lives in #9's interpreter and is explicitly out
of scope here. This makes every statement dry-runnable, golden-testable, and
composable without credentials.

## Scope

In scope:
- Pure `eval(stmt, &EvalCtx) -> Result<Value, EvalError>` over the typed AST.
- Query-stage folding: each `|>` operator maps to a logical `PlanSource`/relation
  node (`Scan(path)`, `Filter`, `Project`, `Extend`, `Join`, `Union`, `Except`,
  `Aggregate`, `Expand`, `Decode`, …) — a *description*, never an executed scan.
- Write operators + `CALL` constructing `EffectNode`s with declared dependencies
  and the `irreversible` flag, attached into the `Plan` DAG (#9 types).
- Alias-function expansion (`SEND`, `MERGE`) via receiver-typed resolution to `CALL`.
- Capability gating + procedure resolution against the driver declaration (parse/eval
  time rejection with structured errors).
- Pure expression evaluation for predicates/projections referenced in plan nodes.

Out of scope (deferred):
- The `Plan`/`EffectNode`/`PlanSource` type definitions, `PREVIEW` rendering and the
  impure `COMMIT` interpreter → **#9**.
- Actual driver I/O, HTTP clients, pushdown execution, batching/parallelization → runtime/driver tickets (E2/E4).
- Federation/local-combine engine choice (DuckDB vs own evaluator) → E3.
- Grammar/AST and registries themselves → **#4**; type/schema model → **#5**.

## Key components

New crate/module `cfs-eval` (domain, no I/O deps), touching `cfs-plan` (#9),
`cfs-ast` (#4), `cfs-types` (#5):

- `fn eval(stmt: &Stmt, ctx: &EvalCtx) -> Result<Value, EvalError>` — entry point.
- `enum Value { Relation(PlanSource), Plan(Plan), Scalar(Literal) }` — owned, no
  vendor types.
- `struct EvalCtx<'a> { registry: &'a Registry, drivers: &'a DriverCatalog,
  caps: &'a CapabilityIndex, schema: &'a SchemaEnv }` — read-only views; carries the
  three open registries (paths/functions/codecs) and capability/procedure declarations.
- `fn fold_query(from: &FromClause, stages: &[Stage], ctx) -> Result<PlanSource, EvalError>`
  — left-fold pipe stages into a logical relation node.
- `fn eval_write(op: &WriteOp, input: PlanSource, ctx) -> Result<Plan, EvalError>`
  — `INSERT/UPSERT/UPDATE/REMOVE` → `EffectNode` (path = type; `RETURNING` projection).
- `fn eval_call(call: &CallExpr, input: Option<PlanSource>, ctx) -> Result<Plan, EvalError>`
  — resolve `driver.action` against declared procedures; error if undeclared.
- `fn expand_alias(name, recv_driver, ctx) -> Option<FnBody>` — receiver-typed alias
  resolution; ambiguity falls back to qualified `CALL` (RFD §3).
- `trait PureFn { fn apply(&self, args, ctx) -> Result<Value, EvalError>; }` — every
  function is `… -> Plan`/relation; the trait cannot perform I/O (no `World` in signature).
- `enum EvalError { CapabilityDenied{path,verb}, UnknownProcedure(String),
  TypeMismatch{..}, UnboundColumn(String), AmbiguousAlias(String), ArityMismatch{..} }`
  — structured, AI-consumable (RFD §5 capabilities).

## Implementation steps

1. Define `Value` and `EvalCtx` over the #9 `Plan`/`PlanSource` and #5 `SchemaEnv`.
2. Implement pure expression eval for `WHERE`/`SELECT`/`EXTEND`/`SET` operand trees
   (operators `= <> < > <= >= AND OR NOT LIKE ~ ANY IN BETWEEN`), against column env.
3. Implement `fold_query`: one arm per query keyword producing a `PlanSource` node;
   thread schema so each stage's output schema is computed (uses #5 inference).
4. Wire `DECODE`/`ENCODE` as pure `PlanSource` nodes (codec registry lookup only).
5. Implement `eval_write` for the four universal verbs → `EffectNode` with deps on
   the input `PlanSource` and `irreversible` set from the driver capability decl.
6. Implement `eval_call` + procedure resolution; reject undeclared procs and
   capability-violating verbs with structured `EvalError`.
7. Implement `expand_alias` (prelude functions, e.g. `SEND`) with receiver-typed
   resolution and qualified-`CALL` fallback.
8. Assemble multi-statement / piped-into-write plans into a single `Plan` DAG.
9. Golden tests: statement → serialized plan/relation snapshot (no creds).

## Considerations

- **Purity invariant is the load-bearing property.** No signature in `cfs-eval` may
  take or return `World`, an HTTP client, or a token; enforce via crate-level deny of
  I/O dependencies (no `reqwest`/`tokio` in `cfs-eval`'s `Cargo.toml`). This is what
  makes `SEND` safe and everything dry-runnable.
- **Least privilege / secrets:** the evaluator never sees credentials; capability
  gating happens here so denied verbs never reach a plan. Keep `EvalError` messages
  free of secret/path-sensitive data beyond the path being operated on.
- **Idempotency/recovery:** carry the `irreversible` flag and verb identity onto each
  `EffectNode` (e.g. `UPSERT` marked retry-safe) so #9/runtime can preview and recover;
  the evaluator establishes these flags but does not act on them.
- **Determinism for golden tests:** plan/relation node IDs and ordering must be stable
  given a fixed AST input (deterministic fold order, no map-iteration nondeterminism).
- **Hard part — receiver-typed alias resolution & ambiguity:** resolve a prelude alias
  only for plans whose driver provides it; on ambiguity emit `AmbiguousAlias` and
  require the qualified `CALL`. Cover with focused tests (alias present on one driver,
  on two, on none).
- **Hard part — schema threading through stages:** each operator must produce a correct
  output schema for the next stage and for `RETURNING`; reuse #5 inference rather than
  re-deriving types, to keep one source of truth.
- **Owned DTOs / no vendor leak (RFD §9):** `PlanSource`/`EffectNode` reference paths
  and column names, never driver SDK structs.
- **Directory/standards:** domain-only crate, `#![forbid(unsafe_code)]`, `thiserror`
  for `EvalError`, doc-comments on each public fn; follow workspace clippy config.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; `cargo test -p cfs-eval` passes.
- `cfs-eval` has **no** runtime/HTTP/async dependency (verified by `Cargo.toml`/`cargo tree`).
- A pure query statement evaluates to a `Value::Relation(PlanSource)` whose node tree
  matches a golden snapshot; **no I/O occurs** (assert via a panicking dummy driver if invoked).
- `INSERT/UPSERT/UPDATE/REMOVE` and `CALL mail.send` evaluate to `Value::Plan` with the
  expected `EffectNode`s, dependency edges, and `irreversible` flags (plan assertions).
- `SEND(d)` desugars to the same plan as `d |> CALL mail.send` (golden equivalence).
- Capability/procedure violations (`REMOVE` on a read-only node; undeclared proc;
  ambiguous alias) return the specific structured `EvalError` — asserted by tests.
- No live credentials or network access used anywhere in the test suite.
