---
created_at: 2026-06-27T12:01:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626103000-t70-operator-equals-binds-eqeq-compares.md, 20260627120000-t73-resource-literal-drop-from.md]
---

# t74 — Lowercase the closed keyword set

## Overview

Implements roadmap **decision S** (Part 1.2): keywords are **lowercase** (`where`, `select`, `let`,
`insert into`, `join`, `policy`, …). Paths, column names, and bindings carry the visual weight; the
closed keyword set stays quiet. Today `crates/lang/src/lex.rs` documents keywords as "case-sensitive
UPPERCASE" and `keywords.rs` matches uppercase verbatim (no folding). This is a **readability decision,
not a new capability** — only the keyword set changes; identifiers/paths stay case-sensitive data.

**Open decision to settle in this ticket:** strictly-lowercase canonical (reject uppercase) vs
**case-insensitive accept, lowercase canonical render** (recommended — accept any case, emit lowercase
in the generated reference and migrate examples). Recommended path avoids a hard break and lets the
corpus migrate as a rendering sweep.

## Exact seams

- `crates/lang/src/keywords.rs` — keyword matching: fold case (or define canonical lowercase). Update
  the keyword freeze/golden tests **deliberately** to the lowercase spelling.
- `crates/lang/src/lex.rs` — update the "case-sensitive UPPERCASE" doc + the scanner's keyword lookup
  to the chosen case policy.
- `crates/lang/src/reference.rs` + `crates/qfs/src/docs.rs` — render keywords lowercase in
  `docs/language.md`; keep `gen-docs --check` green.
- Corpus + assets: `crates/test/src/parse_golden.rs`, golden snapshots, `crates/skill/assets/SKILL.md`,
  `docs/guide/*`, `docs/cookbook/*`, `docs/query-cookbook.md` — one lowercase migration pass; re-run the
  cookbook retag.

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `gen-docs --check`).

1. **Decide + lexer.** Settle the case policy; implement keyword matching accordingly; lexer
   round-trip tests for lowercase (and uppercase if accepted).
2. **Freeze + reference.** Update the keyword freeze/golden to lowercase; regenerate `docs/language.md`.
3. **Migrate corpus + docs.** Lowercase keywords across the test corpus, goldens, `SKILL.md`, and the
   doc examples; re-run `roadmap_cookbook.rs` retag; bump `BASELINE_CORE` if core grew.

## Key files

- `crates/lang/src/{keywords.rs,lex.rs}` + freeze tests.
- `crates/lang/src/reference.rs`, `crates/qfs/src/docs.rs`, `docs/language.md` (regen).
- `crates/test/src/parse_golden.rs`, `crates/skill/assets/SKILL.md`, `docs/cookbook/*`,
  `docs/query-cookbook.md`, `docs/roadmap.md` (migration).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **One migration pass.** Land in the M6 batch after [[t70 — Operator split: `=` always binds, `==` compares]] and [[t73 — Resource literal: drop `from`, unquote path/`policy`/`member_of` literals]] so `==`, no-`from`, and lowercase all migrate the corpus once (decision S note + the §1.2 footnote).
- **Deferral is rendering-only.** Decision S's deferral covers example *rendering*; the lexer change itself is real and ships here (today's binary does not parse lowercase if strictly-cased).
- **Honesty.** Until this ships, §1.1 and the cookbook keep uppercase; do not document lowercase as working before the slice lands.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
