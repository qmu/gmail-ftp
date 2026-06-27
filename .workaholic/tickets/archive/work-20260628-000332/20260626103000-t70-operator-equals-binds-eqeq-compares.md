---
created_at: 2026-06-26T10:30:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: M
commit_hash: ccc358a
category: Changed
depends_on: []
---

# t70 — Operator split: `=` always binds, `==` compares

## Overview

Implements roadmap **decision O** (Part 1.2): **unlike SQL, a single `=` is never equivalence.** `=`
is reserved for assignment/binding everywhere — `LET x = …`, `EXTEND col = …`, `SET col = …`,
`UPDATE … SET …` — and equivalence becomes the explicit **`==`**. The motivation is M6: once `LET`
and lambdas (t60/t61) make binding a first-class, everyday act, overloading `=` for both "bind a
name" and "is equal" is a footgun. This is a **deliberate, one-time change to the frozen operator
vocabulary** (the closed-core thesis allows owner-decided vocabulary events; it is not an open
extension). It is a **breaking grammar change** for `WHERE`/`JOIN ON`/predicates that today use `=`.
Should ship in the same M6 batch as (ideally just before) **t60 — `LET` binding**, so the binding
operator is unambiguous the moment `LET` lands.

## Exact seams

- `crates/lang/src/token.rs` — `Token` enum. Today a single `=` lexes to one token (`Token::Eq`)
  used for BOTH comparison and the `EXTEND`/`SET` assignment. Add `Token::EqEq` (`==`); keep
  `Token::Eq` (`=`) as the **assignment/binding** token only. (Named args already use `Token::Arrow`
  `=>`, unaffected; `||` concat and `.` nav unaffected.)
- `crates/lang/src/lex.rs` — the scanner must lex `==` as one token (maximal munch) before falling
  back to a single `=`. Mirror how `<=`/`>=`/`<>` are scanned. `=>` (Arrow) must still win over `==`
  where applicable (i.e. `=` followed by `>` is Arrow; `=` followed by `=` is EqEq; lone `=` is Eq).
- `crates/lang/src/keywords.rs` — the **frozen** `OPERATORS` slice (currently 15, includes `=`) and
  the governance tests `operator_count_is_frozen` / the operator golden fixture. Replace the `=`
  comparator entry with `==` (count stays 15; `=` is reclassified as the assignment token, like
  `=>`/`||`/`.` which are punctuation, not comparison operators). Update the golden + count test
  **deliberately** — this is the intended vocabulary change, the freeze test is the tripwire.
- `crates/parser/src/grammar.rs` — the comparison/expression grammar. The equality comparator in
  `comparison`/`expr` must match `Token::EqEq`, NOT `Token::Eq`. The assignment paths keep
  `Token::Eq`: `assignment_list` (EXTEND/SET), `update_stmt`'s `SET`. `named_arg_kv` keeps `=>`.
  `JOIN … ON <expr>` predicates flow through the same comparison grammar, so `ON a == b`.
- `crates/parser/src/grammar.rs` error surface — the `expected:` sets and the token-kind labeller
  (`Token::Eq => "=`"`, add `Token::EqEq => "`==`"`) so parse errors name the right token (RFD §5
  AI-facing contract).
- Generated reference: `crates/lang/src/reference.rs` + `crates/qfs/src/docs.rs` render the operator
  table into `docs/language.md` via `cargo run -p xtask -- gen-docs`. Regenerate; the anti-drift
  `gen-docs --check` must stay green.
- Governance/AST goldens: `crates/test/src/parse_golden.rs` corpus and any `crates/parser` golden
  snapshots that encode `=` comparisons reshape — re-snapshot intentionally.

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`).

1. **Lexer + token.** Add `Token::EqEq`; scan `==` (maximal munch, after the `=>` check). Unit-test
   that `=`, `==`, `=>` lex to `Eq`, `EqEq`, `Arrow` respectively. No grammar change yet.
2. **Grammar.** Point the equality comparator at `Token::EqEq`; keep `Token::Eq` for
   `assignment_list` / `update_stmt` SET. Update the error-expected sets + token labeller. Add parse
   tests: `WHERE a == b` parses; `WHERE a = b` now **fails** with a clear message ("use `==` for
   equivalence; `=` binds"); `EXTEND c = expr` / `UPDATE … SET c = v` still parse.
3. **Governance.** Update `OPERATORS` (`=` → `==`), the operator golden fixture, and
   `operator_count_is_frozen` (still 15). This is the deliberate freeze edit.
4. **Migrate the corpus + assets.** Sweep `=`→`==` for every COMPARISON in the workspace test
   corpus, golden fixtures, `crates/skill/assets/SKILL.md`, the cookbook/handwritten docs, and the
   `docs/guide`/`docs/cookbook` examples — WITHOUT touching `=` used for EXTEND/SET/UPDATE binding.
   Regenerate `docs/{language,…}.md`.
5. **Roadmap + cookbook.** Migrate the roadmap's remaining 🧭 proposed examples to `==` comparisons,
   and re-run the cookbook retag (`packages/qfs/crates/test/tests/roadmap_cookbook.rs`
   `retag_cookbook_grammar_by_parse_result`) so `grammar=core`/`extended` tags re-settle; bump
   `BASELINE_CORE` to the new core count (it should be RESTORED/raised once `==` parses, since the
   federation/read recipes can move to the real grammar).

## Key files

- `crates/lang/src/{token.rs,lex.rs,keywords.rs}` (lexer + frozen operator set + freeze tests).
- `crates/parser/src/grammar.rs` (comparison vs assignment; error labels) + parser tests.
- `crates/lang/src/reference.rs`, `crates/qfs/src/docs.rs`, `docs/language.md` (generated operator
  reference; regenerate, do not hand-edit).
- `crates/test/src/parse_golden.rs` + golden snapshots; `crates/skill/assets/SKILL.md`;
  `docs/roadmap.md`, `docs/guide/*`, `docs/cookbook/*` (example migration).
- `crates/qfs/Cargo.toml` version bump (patch) for the shipping PR.

## Considerations

- **Closed-core governance.** This swaps one operator for another in the frozen set — a deliberate
  vocabulary event, not an open extension. The freeze tests are updated in the same PR that makes the
  change, so the tripwire still guards against *accidental* drift afterward.
- **Breaking change — honesty + sequencing.** Every existing `WHERE a = b` stops parsing. Land this
  with (or immediately before) [[t60 — LET binding]] in the M6 batch, migrate all in-repo examples in
  the same PR, and call it out in the release note. Until it ships, the binary still uses `=` for
  comparison — do not document `==` as working before the slice lands (the cookbook's `grammar=core`
  recipes keep `=` until then; the retag flips them when `==` is real).
- **Error quality (RFD §5).** A stale `=`-as-comparison should fail with an actionable, secret-free
  message steering to `==`, since AI agents and humans will both hit it during migration.
- **No `=`/`==` ambiguity with `=>`.** Lock lexer precedence: `=>` (named arg / lambda) and `==`
  (equivalence) and `=` (bind) must each be unambiguous; cover all three in a lexer round-trip test.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag on ship (CLAUDE.md).
