---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260622214650-t03-lexer-tokenizer.md]
---

# Grammar + AST (pipe-SQL core) and closed-core/three-registry governance

## Overview
This ticket delivers the **frozen pipe-SQL grammar** and the **owned AST** that every other
subsystem (effect-plan, runtime, drivers, server DDL) consumes. It implements RFD §2.2
(pipe-SQL), §3 (closed core + three open registries + purity invariant), §4 (data/type model:
`EXPAND`, path access, `@version`, codecs), and the parser-stack decision in §9 (winnow).

The point of cfs is that an AI learns **one small grammar** instead of N SDKs (RFD §1). This
ticket is where that grammar is pinned down as a reserved-word set and a sum-type AST, so that
"new backend = zero new keywords" (RFD §3) is enforced *structurally* — there is no AST node a
driver can add. Everything a driver contributes flows through three open namespaces (paths,
functions/`CALL`, codecs) that the grammar already has slots for.

## Scope
In scope:
- Token-stream → AST parser for the full closed-core statement grammar (query, effect, codec,
  plan, server-DDL forms listed in RFD §3).
- The `ast` module: owned, vendor-free sum types for statements, pipe ops, expressions, paths.
- Reserved-word table (RFD §3) and parse-time rejection of reserved words as identifiers.
- Governance encoding: `CALL driver.action(...)`, `fn(...)`, `DECODE/ENCODE fmt` parse into
  *open* registry-reference nodes (names are strings resolved later, not grammar).
- Structured `ParseError` with span + expected-set (AI-consumable per RFD §5/§10).
- Golden-AST snapshot tests over a statement corpus.

Out of scope (deferred):
- Lexer/tokenizer itself → **t03** (this ticket consumes its tokens).
- Name resolution / capability gating / function & codec *registries* (semantic phase) →
  effect-plan/runtime tickets (E2). We parse `CALL mail.send` into a node; we do **not** check
  that `mail` declares `send`.
- Effect-plan DAG construction and `PREVIEW/COMMIT` execution (E2).
- Pushdown/federation analysis (E3); driver prelude alias expansion (E4).

## Key components
New crate/module `cfs-lang` (or `crates/lang`), modules `ast`, `parse`, `keywords`.

- `ast::Statement` — top sum type:
  ```rust
  enum Statement {
      Query(Pipeline),                 // FROM <path> |> op |> op ...
      Effect(EffectStmt),              // INSERT/UPSERT/UPDATE/REMOVE ... [RETURNING]
      Ddl(ServerDdl),                  // CREATE ENDPOINT|TRIGGER|JOB|VIEW|... 
      Plan(PlanWrap),                  // PREVIEW <stmt> | COMMIT <stmt>
  }
  ```
- `ast::Pipeline { source: Source, ops: Vec<PipeOp> }`; `Source` = `Path(PathExpr)` or a
  subquery / `VALUES`.
- `ast::PipeOp` — one variant **per closed-core query/transform keyword only**:
  `Where, Select, Extend, Set, Aggregate, GroupBy, OrderBy, Limit, Distinct, Join, Union,
  Except, Intersect, As, Expand, Decode(Codec), Encode(Codec), Call(CallRef)`.
- `ast::EffectStmt` — `Insert{into: PathExpr,..}, Upsert{..}, Update{..}, Remove{..}` each
  carrying optional `Values`, source pipeline, and `returning: Option<Vec<Expr>>`.
- `ast::Expr` — `Lit, Col, Path(a.b.c struct nav), FnCall(FnRef), Binary(Op,..), Unary,
  In, Between, Like, AnyOp` (operators frozen per RFD §3); `ast::Op` enum for
  `= <> < > <= >= AND OR NOT LIKE ~ ANY IN BETWEEN`.
- `ast::PathExpr` — `/driver/seg/seg`, with optional `@version` coordinate and `AS OF <ts>`
  (RFD §4); driver + segments are *strings* (open path registry).
- Open-registry reference nodes (the three governance seams, all name-as-string):
  `CallRef { driver: Ident, action: Ident, args: Vec<NamedArg> }`,
  `FnRef { name: Ident, args: Vec<Expr> }`, `Codec { fmt: Ident }`.
- `ast::ServerDdl` — `Endpoint, Trigger, Job, View{materialized:bool}, Webhook, Policy`
  with `DO/EVERY/ON` clauses; parsed as **sugar** — AST notes these desugar to
  `INSERT INTO /server/...` (RFD §8) but desugaring lives downstream.
- `keywords::RESERVED: &[&str]` — frozen set from RFD §3; `keywords::is_reserved(&str)`.
- `parse::parse_statement(tokens) -> Result<Statement, ParseError>` (winnow combinators over
  the t03 token slice); `ParseError { span, expected: Vec<&str>, found }`.

Invariants honored: closed-core grammar (no extensibility hook in `PipeOp`/`Statement`);
three open registries (only string-named refs); purity (AST is data — no execution); owned DTOs
(zero vendor types in `ast`); capability gating deferred but *enabled* by structured `CallRef`.

## Implementation steps
1. Add `cfs-lang` crate; depend on `winnow` and the t03 lexer crate; re-export `Token`.
2. Encode `keywords::RESERVED` from RFD §3 verbatim (query, effect, codec, plan, DDL,
   operators); add `is_reserved` + a unit test asserting the exact frozen list.
3. Define `ast` sum types above; derive `Debug, Clone, PartialEq, serde::Serialize`
   (Serialize powers `-json` AST dumps and golden tests).
4. Build expression parser (Pratt/precedence-climbing over the frozen operator set).
5. Build `PathExpr` parser incl. `@version` / `AS OF`, and `a.b.c` struct navigation in exprs.
6. Build pipeline parser: `FROM <source>` then `|>`-separated `PipeOp`s; one combinator per op.
7. Build effect-statement parser incl. `VALUES`, `RETURNING`, sub-pipeline source.
8. Build codec ops (`DECODE/ENCODE fmt`) and `CALL driver.action(args)` / `fn(...)` as
   open-registry reference nodes (validate *shape*, never resolve names).
9. Build server-DDL parser (`CREATE … DO/EVERY/ON`); tag each with its `/server/...` target.
10. Wrap with `PREVIEW`/`COMMIT` at statement top level.
11. Implement `ParseError` with span + expected-set; reject reserved-word-as-identifier with a
    targeted message.
12. Assemble golden corpus + snapshot tests; wire `cargo clippy -D warnings`.

## Considerations
- **Governance is the hard part.** The temptation is to special-case popular actions (`SEND`,
  `MERGE`) into the grammar. RFD §3 forbids this: they are pure registry functions desugaring to
  `CALL`. Resolution: `PipeOp` has **no** per-action variants; only `Call(CallRef)` and
  `Fn`-bearing `Expr`. A code-review/test gate asserts the `PipeOp`/`Statement` variant count is
  fixed so new keywords can't slip in.
- **Frozen list as a test, not a comment** — golden test pins `RESERVED` exactly; changing it is
  a deliberate, reviewed event (operation: change-control / least surprise).
- **Span fidelity & AI ergonomics** — every node carries source spans; `ParseError.expected`
  must be a real set so an agent can self-correct (RFD §5 "structured error … important for AI",
  §10). Test error messages as golden output too.
- **Purity/idempotency** — parser does zero I/O and is deterministic; `INSERT` vs `UPSERT` is
  preserved distinctly in the AST so the runtime can pick retry-safe verbs (RFD §6).
- **Owned DTOs / no leaks** — `ast` depends on no driver/vendor crate; this is the boundary that
  keeps SDK types out of the core (RFD §9).
- **Secrets** — none handled here; ensure error `Debug`/serialization never echoes literal
  values from credential-bearing statements (redact `Lit` in error display).
- **Directory/standards** — follow `crates/<name>/src/{ast,parse,keywords}.rs`; public surface
  is `parse_statement` + `ast` types only.
- **Recovery** — winnow chosen (RFD §9); if DSL error-recovery proves decisive, the parser API
  (`tokens -> Result<Statement>`) is stable enough to swap to chumsky behind it.

## Acceptance criteria
- `cargo build` and `cargo clippy -D warnings` are green; no vendor/driver deps in `cfs-lang`.
- Unit test asserts `keywords::RESERVED` equals the exact RFD §3 frozen set; using any reserved
  word as an identifier is a parse error with a targeted message.
- Round-trip / golden-AST snapshot tests pass for a corpus covering: a multi-op `FROM |> WHERE
  |> SELECT |> JOIN |> AGGREGATE |> ORDER BY |> LIMIT` query; `EXPAND`; `DISTINCT`;
  `UNION/EXCEPT/INTERSECT`; `INSERT/UPSERT/UPDATE/REMOVE … VALUES … RETURNING`;
  `DECODE/ENCODE fmt`; `CALL mail.send(...)`; a registry `fn(...)`; `@version` and `AS OF`;
  struct path access `a.b.c`; each `CREATE ENDPOINT|TRIGGER|JOB|VIEW|MATERIALIZED VIEW|WEBHOOK|
  POLICY` form with `DO/EVERY/ON`; and `PREVIEW`/`COMMIT` wrappers.
- A governance test asserts the `Statement`/`PipeOp` variant set is exactly the closed core
  (no per-driver/per-action variants); driver-specific behavior appears **only** as string
  names inside `CallRef`/`FnRef`/`Codec`/`PathExpr`.
- `ParseError` carries span + non-empty `expected` set; golden tests cover representative error
  cases. **No live credentials** are needed for any test (pure, in-process).
