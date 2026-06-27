---
created_at: 2026-06-27T12:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626103000-t70-operator-equals-binds-eqeq-compares.md]
---

# t73 — Resource literal: drop `from`, unquote path/`policy`/`member_of` literals

## Overview

Implements roadmap **decision R** (Part 1.2): a `/path` is a first-class **`Resource`** value — a
lazy, describable handle to a node — and the source position **needs no `from`**. A leading `/path`
*is* the source, and the **same** literal serves `join`/set-op operands, `policy` targets, and
`member_of(…)` — one spelling for "a node" instead of four (today: `from /path`, bare `/path` after
`join`, quoted `'/path'` in `policy`, quoted string in `member_of`). A leading `/` stays unambiguous by
**position** — `/` where an expression *begins* (a stage or operand start) is a path; `/` *between* two
operands is division — so this composes with infix arithmetic (t75) without a clash. This is a
**breaking grammar change** (every `FROM …` and quoted `policy`/`member_of` path must migrate) and a
deliberate removal of `FROM` from the frozen keyword set. Ships in the M6 batch alongside t70/t74.

## Exact seams

- `crates/lang/src/keywords.rs` — remove `FROM` from the frozen keyword set; update the keyword-count
  freeze test **deliberately** (this is the intended vocabulary event, the freeze test is the tripwire).
- `crates/lang/src/lex.rs` — a `/`-led token at an **expression-start** position lexes as a path; a `/`
  in **infix** position is the division operator (the regex-vs-divide rule — track lexer/parser state).
- `crates/parser/src/grammar.rs` — the `Source` production: a statement/stage that begins with a `/path`
  or a bound identifier IS the source (no `FROM`). `policy` `ON <path>` and `member_of(<path>)` accept a
  bare path literal, not a quoted string. Pratt/precedence: `/` is path in prefix position, division in
  infix. Keep the `|>`-only-continuation rule and the multi-clause `create`-statement carve-out
  (decision R, §1.2).
- `crates/core/src/eval.rs` — model a `Resource` value (a lazy handle): pure to hold and `describe`,
  forced to rows only when a pipeline consumes it. A bound name resolving to a `Resource` is a valid
  source.
- `crates/lang/src/reference.rs` + `crates/qfs/src/docs.rs` — regenerate `docs/language.md`; keep
  `gen-docs --check` green.

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `gen-docs --check`).

1. **Keyword removal.** Drop `FROM`; update the freeze/count test. No grammar change yet (parser still
   needs a source — temporarily require the leading path).
2. **Source without `from`.** Parser accepts a leading `/path` / bound identifier as the source.
   Position-based `/` (path at expression-start, division infix); lexer round-trip tests.
3. **Unquote `policy`/`member_of`.** `ON <path>` and `member_of(<path>)` take bare `Resource` literals.
4. **`Resource` value.** Lazy handle in `eval`; describe/hold is pure; pipeline forces rows.
5. **Migrate corpus + docs.** Sweep `FROM`-removal across the test corpus, goldens, `SKILL.md`,
   `docs/guide/*`, `docs/cookbook/*`; re-run the cookbook retag and bump `BASELINE_CORE`.

## Key files

- `crates/lang/src/{keywords.rs,lex.rs,token.rs}` + freeze tests.
- `crates/parser/src/grammar.rs` + parser goldens.
- `crates/core/src/eval.rs` (the `Resource` value).
- `crates/lang/src/reference.rs`, `crates/qfs/src/docs.rs`, `docs/language.md` (regen).
- `crates/test/src/parse_golden.rs`, `crates/skill/assets/SKILL.md`, `docs/roadmap.md`,
  `docs/cookbook/*`, `docs/query-cookbook.md` (migration + retag).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Breaking change.** Every `FROM …` stops parsing. Land in the M6 batch with [[t70 — Operator split: `=` always binds, `==` compares]] and [[t74 — Lowercase the closed keyword set]]; migrate all in-repo examples in the same PR; call it out in the release note.
- **Position-based `/`.** This is the one genuinely tricky lexer point (a known, solved problem — JS distinguishes regex from divide the same way). Cover prefix-path vs infix-divide in a round-trip test; it is what makes dropping `from` safe *and* lets t75 keep infix arithmetic.
- **Newline boundary unchanged.** `|>` is the only pipeline continuation; a multi-clause `create policy`/`create trigger` continues across its clause keywords (§1.2 carve-out).
- **Sequencing.** Operator (t70) first or together; [[t60 — LET binding]] golden tests reference the bare `products` source this ticket delivers; [[t72 — pipe-stage write grammar]] shares the no-`from` write form.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
