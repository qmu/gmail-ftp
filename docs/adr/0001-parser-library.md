# ADR 0001 — Parser library for the qfs pipe-SQL DSL

- **Status**: Accepted (locked)
- **Date**: 2026-06-22
- **Deciders**: qfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t02 — parser-library decision spike
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §2 (Pipe-SQL), §3 (closed core + frozen keywords/operators),
  §5 (AI-critical *structured* errors), §9 (Implementation: parser research, owned-DTO /
  no-vendor-leak rule).

## Decision

**qfs uses [`winnow`](https://crates.io/crates/winnow) (1.0.3) as the DSL parser
library.** chumsky (0.13.0) was the only serious challenger (it can do parse-error
recovery — the one capability that could have overridden the RFD §9 default); combine
was excluded up front (sporadic maintenance, RFD §9). winnow's recovery story is not
decisive enough to lose to, and it wins every other criterion, so the §9 default stands.

The library is confined to the crate-private `qfs-parser::grammar` module. The public
surface is the owned `ParseError` (byte span + expected-set + machine code) and owned AST
types; **no winnow type appears in `qfs-parser`'s public API** (fidelity guard G6 / RFD §9
owned-DTO rule). This keeps the choice reversible.

## Context

The DSL parser is the front door of the whole engine (RFD §2/§9): it turns DSL text into
the AST sum types that justified Rust over Go, and every downstream epic (E1 language core,
E2 effect-plan, E7 server DDL) builds on it. RFD §9 names **winnow** the default
(maintained — commits this week; function/combinator-based, no macros; fastest; pure-Rust)
and **chumsky** the fallback *only if* parse-error recovery proves decisive (RFD §5: the
AI structured-error path is load-bearing). We did not take §9 on faith. We built a spike
(`spikes/parser-spike`, `publish = false`, throwaway) that parses the identical subset
grammar `FROM <path> |> WHERE <expr> |> SELECT <cols>` (UPPERCASE keywords, `|>` pipes)
in **both** libraries into **one shared AST**, then compared on the ticket's criteria with
a committed golden error corpus (`spikes/parser-spike/tests/golden/errors.txt`) and a
cross-parser AST-equality test.

## Comparison (evidence, not opinion)

| Criterion (weight) | winnow 1.0.3 | chumsky 0.13.0 | Winner |
|---|---|---|---|
| **Error machineability** (highest — AI structured-error path, RFD §5) | Token-level expected-sets: `FROM, \|>, WHERE, SELECT, AND, a path`. Clean `UNKNOWN_KEYWORD` classification for lowercase. | Char-level expected-sets: `'W', 'S'`, `'E'`, `'F'` — leaks char-by-char matching internals; less useful for an AI consumer. | **winnow** |
| **Multi-error recovery** | None out of the box (single error). | Recovery *available* but requires explicit `.recover_with(...)`; out of the box still 1 error/input (`chumsky-recovery-count: 1` for every corpus case). | tie (neither free); chumsky's potential noted |
| **Span / position fidelity** | Precise with `cut_err` on committed alternatives (byte 13 for lowercase `where`, 13 for `SHUFFLE`, 26 for the unterminated string, 18/35 for the EOF cases). | Precise (furthest-error tracking): byte 13, 14, 39, 18. Comparable. | tie |
| **`\|>`-chain & UPPERCASE ergonomics** | Combinators (`preceded(ws("\|>"), cut_err(pipe_op))`) read directly; UPPERCASE = plain string literals; sourced from the frozen `qfs_lang::Keyword` set. | Equally natural (`just("\|>")`, `choice`, `padded`). | tie |
| **Build / compile cost** (tiebreaker, RFD §1/§9 Workers footprint) | **Zero transitive deps.** Clean compile **~1.34 s**; rlib ~2.99 MB (debug). | Pulls `hashbrown`, `stacker` (+`psm`, a **C-built** stack manipulator via `cc`), `libc`, `unicode-*`. Clean compile **~4.70 s** (3.5×); rlibs ~6.83 MB total (debug). | **winnow** |
| **wasm32 footprint** (CI-only / deferred — see below) | Pure-Rust, macro-free, no platform deps → known-smaller, wasm-clean. | `stacker`/`psm` do platform stack manipulation with a C build dependency — **wasm-hostile**; needs feature-gating to build for `wasm32`. | **winnow** |
| **Maintenance / risk** (RFD §9) | Actively maintained (commits this week). | On Codeberg, GitHub archived; published on crates.io. Lower velocity. | **winnow** |
| **Parse throughput** (lowest weight) | Known-fastest (RFD §9); both trivially fast on the subset grammar — not a differentiator here. | Fast enough. | tie (winnow per §9) |

Golden corpus excerpt (full file: `spikes/parser-spike/tests/golden/errors.txt`):

```
## case: lowercase_keyword | input: "FROM mail |> where id = 1"
winnow:  [UNKNOWN_KEYWORD] at byte 13 | expected: UPPERCASE keyword | expected UPPERCASE keyword, found `where`
chumsky: [UNEXPECTED_TOKEN] at byte 13 | expected: ''W'', ''S'' | found 'w' expected 'W', or 'S'
chumsky-recovery-count: 1

## case: unknown_op | input: "FROM mail |> SHUFFLE id"
winnow:  [UNEXPECTED_TOKEN] at byte 13 | expected: FROM, |>, WHERE, SELECT, AND, or a path | unexpected token near `SHUFFLE`
chumsky: [UNEXPECTED_TOKEN] at byte 14 | expected: ''E'' | found 'H' expected 'E'
chumsky-recovery-count: 1
```

Cross-check: on all valid corpus inputs both parsers produced the **identical** shared AST
(`winnow_and_chumsky_agree_on_valid_inputs`, green).

### The genuinely hard call

The *only* RFD-sanctioned reason to override the winnow default is if chumsky's
parse-error **recovery** is decisive for the AI structured-error path (RFD §5). The
evidence says it is **not decisive**:

1. Recovery is **not free** in chumsky either — it requires explicit `.recover_with(...)`
   strategies per production. Out of the box both libraries surface one error per input.
   The same multi-error behaviour is achievable in winnow with comparable effort
   (re-parse / synchronise on `|>` boundaries), since the DSL is short and pipe-delimited.
2. For *machineability* (the actual §5 requirement: span + expected-set + **code**),
   winnow's **token-level** expected-sets (`WHERE, SELECT, |>`) are strictly more useful to
   an AI consumer than chumsky's **char-level** expected-sets (`'W'`, `'S'`). chumsky's
   recovery would surface *more* errors, but each in the lower-quality char-level form.
3. qfs statements are short, single-line pipelines, not large source files; multi-error
   recovery (chumsky's headline strength) has low marginal value here versus one precise,
   well-classified, well-spanned error.

So recovery does not clear the bar to override §9, and winnow's footprint/maintenance/
expected-set advantages are decisive on top.

## wasm32 build size — CI-only / deferred (recorded, not measured locally)

`wasm32-unknown-unknown` is **not installed** on the dev host and `rustup target add` is a
user-toolchain mutation forbidden by system-safety; the target is explicitly deferred
(trip assumption A2 / risk R5). We therefore **did not** add the target. The wasm32 build
of `qfs-parser` (`cargo build -p qfs-parser --target wasm32-unknown-unknown`) is **deferred**:
CI carries a parked, commented-out placeholder (`.github/workflows/ci.yml`, the wasm32 step),
to be activated by the E0 wasm32 sibling ticket — it is not yet an active CI gate. Qualitative datapoint: winnow is
pure-Rust, macro-free, with **zero** transitive deps → known-smaller and wasm-clean;
chumsky's `stacker`/`psm` (C-built platform stack manipulation) is wasm-hostile and would
need feature-gating. This corroborates the winnow choice and is not a close call, so the
deferred numeric size-delta does not jeopardise the decision.

## Alternatives considered

- **chumsky 0.13.0** — see comparison. Strong recovery story, but not decisive for short
  pipe-SQL; heavier deps; wasm-hostile transitive (`psm`); lower maintenance velocity.
- **combine** — excluded per RFD §9 (sporadic maintenance, ~4.5 months idle at research
  time). Not spiked.
- **hand-written recursive-descent** — maximum control, but reinvents span/expected-set
  plumbing winnow already provides; rejected for E0 (revisit only if a library boundary
  becomes a real constraint).

## Consequences

- `qfs-parser` takes a real `winnow` dependency, used **only** inside the private
  `grammar` module. Public API stays owned (`ParseError`, AST). Asserted by
  `qfs-parser::tests::no_vendor_type_in_public_api` and the module being non-`pub`.
- The frozen keyword surface text is sourced from `qfs_lang::Keyword` (RFD §3, one home),
  not hand-typed in the grammar.
- E1 grows the grammar from the E0 subset to the full RFD §3 grammar against this same
  boundary; downstream crates never see winnow.
- The two spikes are retained under `spikes/parser-spike` as comparison evidence
  (`publish = false`, banner-marked NOT production). The chumsky spike (the retained loser)
  carries a banner pointing here so it is not misread as a live second parser (A3).

## Reversibility

The owned `ParseError` + owned AST is the escape hatch. Swapping winnow for another library
means: (1) rewrite the private `grammar` module against the new library, (2) re-map its
error into `ParseError`. **No public signature changes**, so E1+ callers (`qfs-core` →
`parse_statement`) are untouched. The `no_vendor_type_in_public_api` test and the private
`grammar` module structurally bound the blast radius of a future swap to this one module.
```
