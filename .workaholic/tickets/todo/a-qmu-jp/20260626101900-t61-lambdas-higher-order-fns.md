---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626101800-t60-let-binding.md]
---

# t61 — Lambdas as values + `map`/`filter`/`reduce` + `DEF`

## Overview
Delivers the heart of **M6 — Language core**: **functions are values** (roadmap §1.2; decision H).
A lambda `(addr: string) => lower(trim(addr))` becomes a first-class expression value, and
`map`/`filter`/`reduce` take such values as arguments — expressing transformations that today need
an external script. The governance-critical point: **none of this adds a keyword.** Lambdas extend
the **expression grammar** (a new `Expr` variant) and `map`/`filter`/`reduce` are ordinary
higher-order entries in the **stdlib registry** — the two open registries (functions, the
`StdlibRegistry`) absorb everything, so the frozen closed core (38 keywords) is **untouched**. This
is the deliberate contrast with t60/t62 (which *do* add keywords). A named function is just a
`LET`-bound lambda (t60), so `DEF` should NOT become a keyword either — flag and prefer the
no-keyword path. The `Expr::Fn`/`FnRef` function seam and the `StdlibRegistry` already exist as a
library; this ticket adds closures and higher-order builtins on top. Pure — no I/O, wasm-buildable.

## Exact seams
- `crates/parser/src/ast.rs` — `Expr` enum (9 variants incl. `Expr::Fn(FnRef)` — the function-call
  registry seam). Add a new `Expr::Lambda { params: Vec<Param>, body: Box<Expr> }` variant (with a
  `Param { name: Ident, ty: Option<TypeAnn> }` so the `(addr: string) =>` annotation parses). This is
  an **expression** change only — `Statement`/`PipeOp` variant sets are NOT touched, so their
  governance tests stay green unchanged. `FnRef` already carries `args: Vec<Expr>`, so a lambda flows
  into `map(col, normalize)` as just another argument expression — no new call machinery.
- `crates/parser/src/grammar.rs` — winnow `parse_statement()` / the expression parser (ADR-0001
  locked grammar): add the `(params) => <expr>` lambda production and the `=>` arrow token. Confirm
  `=>` does not collide with the frozen `OPERATORS` set in `crates/lang/src/keywords.rs` (named-arg
  syntax `to => email` already uses `=>` — reuse the same token; the lambda is distinguished by the
  parenthesised param list, not a new operator).
- `crates/core/src/eval.rs` — `Evaluator`, `eval_expr()`, `EvalValue`. Add a closure value
  (capturing the `LET` environment from t60) so `eval_expr` of an `Expr::Lambda` produces a callable
  value, and applying it binds params and evaluates the body. Add `EvalError` arms for arity
  mismatch / applying a non-function (structured, AI-consumable). Closures capture by the t60 binding
  environment — this is why t61 `depends_on: [t60]`.
- `crates/core/src/stdlib/` — `BuiltinFn`, `FnSig`, `StdlibRegistry` and the
  `mod.rs`/`scalar.rs`/`aggregate.rs`/`tablevalued.rs` split (core builtins + driver preludes; "the
  place higher-order fns/UDFs plug in"). Register `map`/`filter`/`reduce` as higher-order
  `BuiltinFn`s whose `FnSig` accepts a function-typed argument; their implementations invoke the
  evaluator's closure-application path. `split`/`lower`/`trim` (used by the roadmap example) live here
  as scalars — add any missing ones.
- `crates/core/src/resolve.rs` — `Resolver`/`resolve_expr`/`ResolvedCall`. A lambda passed by name
  (`map(col, normalize)`) resolves `normalize` against the t60 `LET` environment first, then the
  stdlib; an unbound function name is the existing structured resolve error.

## Implementation steps
1. **Lambda AST (no keyword).** Add `Expr::Lambda { params, body }` + `Param` to
   `crates/parser/src/ast.rs`; derive the same traits as sibling `Expr` variants. Confirm the
   `Statement`/`PipeOp` governance tests and the `crates/lang` keyword freeze are **untouched** (add
   a comment asserting "no keyword added — functions are values"). `cargo test -p qfs-parser`.
2. **Lambda grammar.** Add the `(params) => expr` production to `crates/parser/src/grammar.rs`,
   reusing the existing `=>` token; parser golden tests for `(addr: string) => lower(trim(addr))` and
   for a lambda as a `FnRef` argument. Keep the named-arg corpus green (disambiguation test).
3. **Closures in the evaluator.** Add a closure `EvalValue` in `crates/core/src/eval.rs` that
   captures the t60 binding environment; implement application (param binding + body eval) and the
   arity/non-function `EvalError` arms. Pure, in-process tests.
4. **Higher-order builtins.** Register `map`/`filter`/`reduce` (and any missing scalars like `split`)
   in `crates/core/src/stdlib/` with `FnSig`s that accept function-typed args; wire them to the
   closure-application path. Golden tests reproducing the roadmap §1.2 `extend recipients = map(...)`
   example end-to-end against an in-memory relation.
5. **`DEF` decision (prefer no keyword).** Implement user-defined functions as `LET`-bound lambdas
   (t60) — `LET normalize = (addr) => …` — so `DEF` adds **zero** keywords and the closed core is
   preserved. Document in the PR that the genuine-keyword `DEF` path was rejected in favour of the
   `LET`-lambda path (flagged below). Run the full gate: `cargo build/test/clippy --workspace`,
   `cargo fmt --all --check`, `cargo run -p xtask -- gen-docs --check` (regenerate `docs/language.md`
   to list the new builtins — never hand-edit). Bump the patch in `crates/qfs/Cargo.toml`.

## Key files
- `crates/parser/src/ast.rs` — modify (`Expr::Lambda`, `Param`; no `Statement`/`PipeOp` change).
- `crates/parser/src/grammar.rs` — modify (lambda production, `=>` reuse).
- `crates/core/src/eval.rs` — modify (closure `EvalValue`, application, `EvalError` arms).
- `crates/core/src/stdlib/{mod,scalar}.rs` — modify (`map`/`filter`/`reduce` + scalars as
  `BuiltinFn`s in `StdlibRegistry`).
- `crates/core/src/resolve.rs` — modify (resolve lambda-by-name against `LET` env then stdlib).
- Generated `docs/language.md` — regenerated via `cargo run -p xtask -- gen-docs` (never hand-edit).

## Considerations
- **Governance — the load-bearing constraint.** This ticket is the proof that the closed core scales
  without new keywords: lambdas + `map`/`filter`/`reduce` ride the **expression grammar** and the
  **`StdlibRegistry`** (decision H, "functions are values"). The `crates/lang/src/keywords.rs`
  freeze tests (`keyword_count_is_frozen` = 38) MUST stay green and unedited; if a change here would
  require a new keyword, that is a design smell — stop and reconsider. The PR must explicitly state
  "zero keywords added."
- **`DEF` open product decision — flag, prefer no keyword.** Per the spec, decide whether `DEF` is a
  `LET`-bound lambda (preferred — no keyword, preserves the closed core) or a genuine keyword. Prefer
  the `LET`-lambda path and document the rejection of the keyword path. If a future need for `DEF`
  surfaces, it would be a *separate, deliberate* keyword-freeze edit like t60/t62, not a quiet add.
- **Purity invariant (RFD §3).** Every function constructs values/plans, never performs I/O; closures
  are pure and dry-runnable. `map`/`filter`/`reduce` over a relation stay in the read/transform half —
  no effect node, so the safety floor (describe pure / preview touches nothing / commit explicit) is
  untouched. Guard against a lambda body that names an effect — keep lambdas expression-only.
- **Dep-direction.** All work is in the pure cores (parser/core); no new crate, no driver/runtime
  edge, tokio stays out, cores stay wasm-buildable (`crates/cmd/tests/dep_direction.rs` unaffected).
- **Type annotations open question.** The `(addr: string)` annotation can be parsed-and-checked, or
  parsed-and-ignored initially. Flag: prefer parse-and-retain (store `Option<TypeAnn>`) so a later
  type-check is non-breaking, but full inference is out of scope for this slice. The actual plan-time
  type checker (and typed literals like `100i64`/`true`) is roadmap decision T — its own
  **static-type-system** ticket (t75), which depends on this `Option<TypeAnn>` parse-and-retain.
  Use lowercase primitive names (`string`, `bool`, `i64`, …) per decision S/T, not Rust `String`.
- **Docs honesty + versioning.** Do not advertise lambdas/`map`/`filter`/`reduce` in `docs/guide/*`
  or the skill until the slice ships and `gen-docs --check` is green (roadmap tags them 🧭). Own PR +
  patch bump in `crates/qfs/Cargo.toml` + `v0.0.x` tag on ship.
