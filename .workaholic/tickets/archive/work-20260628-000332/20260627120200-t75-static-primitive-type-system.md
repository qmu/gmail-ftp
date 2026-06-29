---
created_at: 2026-06-27T12:02:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: L
commit_hash: c832ed2
category: Added
depends_on: [20260626101900-t61-lambdas-higher-order-fns.md, 20260627120000-t73-resource-literal-drop-from.md]
---

# t75 — Static primitive type system, checked at plan time

## Overview

Implements roadmap **decision T** (Part 1.2): a real, **static** type system, checked at **plan time** —
before any I/O, so a type error surfaces in `preview`, consistent with the purity floor. Scalar
primitives are lowercase, Rust-style: **`bool`** (`true`/`false`), fixed-width ints
**`i32`/`i64`/`u32`/`u64`**, floats **`f32`/`f64`**, **`string`** (`'…'`) — beside the **`Resource`**
value (t73) and lambda/function types (CamelCase). Column types come from `describe` (pure), so a whole
pipeline type-checks before it touches the world, and a mismatch (`where total == 'paid'` against an
`i64` column) is a `preview`-time error, never a surprise at commit. Ordinary **infix arithmetic**
(`+ - * /`) is supported and type-checked. Builds on t61's `Option<TypeAnn>` parse-and-retain.

## Exact seams

- `crates/lang/src/lex.rs` + `token.rs` — typed-literal lexing: integer suffixes (`100i64`, `7u32`),
  float suffixes (`3.14f64`), and the `true`/`false` bool literals; `'…'` is `string`.
- `crates/parser/src/grammar.rs` — literal AST nodes carry their primitive type; arithmetic
  (`+ - * /`) expressions parse into typed expr nodes.
- `crates/core` — a **type lattice** (the primitives + `Resource` + function types) and a **checker
  pass** run during plan/preview: infer/check literal types, comparison and arithmetic operand types,
  and bind **column types from each driver's `describe`** contract. A mismatch is a structured,
  secret-free `preview`-time error (RFD §5 AI-facing).
- `describe` contract — ensure each driver's column schema exposes a primitive type (extend the schema
  if a driver returns untyped columns).

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `gen-docs --check`).

1. **Typed literals.** Lex/parse `100i64`, `3.14f64`, `true`/`false`, `'…'`→`string`; unit tests.
2. **Type representation.** A primitive lattice + `Resource`/function types; reuse t61's `TypeAnn`.
3. **Checker pass.** Run in plan/preview: literal + operand typing for comparisons and `+ - * /`;
   structured mismatch errors. No I/O.
4. **Describe → column types.** Bind column types from `describe`; type-check predicates/projections
   against them; cover the `where total == 'paid'` vs `i64` case.
5. **Docs + corpus.** Document typed literals; add type-error golden tests; regenerate reference.

## Key files

- `crates/lang/src/{lex.rs,token.rs}` (typed literals).
- `crates/parser/src/grammar.rs` (typed literal/arith AST) + parser goldens.
- `crates/core/*` (type lattice + plan-time checker; describe→type binding).
- `crates/lang/src/reference.rs`, `docs/language.md` (regen).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Scope.** Start with literal + column + comparison/arithmetic checking; full inference (let-poly,
  lambda inference) is out of scope — note it. Depends on [[t61 — lambdas / higher-order fns]] (TypeAnn) and [[t73 — Resource literal: drop `from`, unquote path/`policy`/`member_of` literals]] (the `Resource` type).
- **Purity floor.** The check runs at plan/preview — before any credential or network use — so a typed
  pipeline fails fast and offline.
- **Infix arithmetic.** Typed `+ - * /`; the leading-`/` path stays unambiguous by position (t73), not
  by banning division (decision T/R).
- **Lowercase names.** Primitive type names are lowercase (`string`, `bool`, `i64`), not Rust `String`
  — align with [[t74 — Lowercase the closed keyword set]].
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
