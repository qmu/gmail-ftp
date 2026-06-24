# E2E Review — t02 qfs-parser skeleton + parser-spike

Author: Planner (Progressive)
Phase: Coding / review-and-testing
Scope: E2E / external-interface testing ONLY (build & exercise from the outside).
No code review, no unit-test authoring — that is the Architect's / Constructor's domain.
Date: 2026-06-23

## What was exercised

The deliverable is `qfs-parser` (`parse_statement(&str) -> Result<Stmt, ParseError>`,
winnow confined behind an owned `ParseError`) plus the `spikes/parser-spike`
winnow-vs-chumsky comparison. I validated both as an external consumer / AI agent would:

- Ran the spike comparison example and the spike tests.
- Built a THROWAWAY out-of-tree consumer crate at `/tmp/qfs-e2e-harness`
  (path-dep on `qfs-parser`, separate `--target-dir`, NOT added to the workspace,
  NO production code touched) and called `qfs_parser::parse_statement` on valid,
  invalid, and adversarial inputs with `RUST_BACKTRACE=1`.
- Read `.github/workflows/ci.yml` to verify the ADR's wasm32-in-CI claim.

## Results per task item

### Item 1 — Run the spike comparison — PASS

`cargo run -p parser-spike --example compare` runs clean and prints the documented
side-by-side winnow-vs-chumsky output for the full golden corpus (5 valid + 7 broken).
`cargo test -p parser-spike` is green: 3/3
(`winnow_and_chumsky_agree_on_valid_inputs`, `both_reject_broken_inputs`,
`golden_error_corpus_is_stable`). The committed golden file
`spikes/parser-spike/tests/golden/errors.txt` matches the live run exactly.

The comparison visibly demonstrates the ADR's decisive criterion (RFD §5): winnow
emits **token-level** expected-sets while chumsky emits **char-level** ones. Sample
(broken `missing_pipe`, input `"FROM mail WHERE id = 1"`):

```
winnow:  [UNEXPECTED_TOKEN] at byte 10 | expected: FROM, |>, WHERE, SELECT, AND, or a path | unexpected token near `WHERE`
chumsky: [UNEXPECTED_TOKEN] at byte 10 | expected: ''|'', end of input | found 'W' expected '|', or end of input
chumsky recovery errors: 1
```

The honest evidence note (A4) is corroborated: chumsky's `recovery-count` is 1 for
every broken case out of the box — multi-error recovery is not free in either library.

### Item 2 — Exercise `qfs_parser::parse_statement` directly — PASS

External harness output (true out-of-tree consumer).

**VALID — grammar-conformant** `FROM mail |> WHERE subject LIKE 'invoice' |> SELECT subject`
→ `Ok(Stmt{..})`:

```
Stmt {
    from: Path(["mail"]),
    ops: [
        Where(Cmp { lhs: Path(["subject"]), op: Like, rhs: Str("invoice") }),
        Select([Path(["subject"])]),
    ],
}
```

**INVALID inputs — all returned `Err(ParseError)` exposing the full RFD §5
machine-readable contract** (byte span `at`, machine `code` via `code.as_str()`,
`expected` set, and `message`). The harness branched purely on `e.code`, never on
prose, confirming the AI-agent contract. Two dumps:

```
[lowercase_keyword] input="FROM mail |> where id = 1"
    code.as_str() = "UNKNOWN_KEYWORD"
    at (byte span) = 13
    expected set   = ["UPPERCASE keyword"]
    message        = "expected UPPERCASE keyword, found `where`"
    Display        = [UNKNOWN_KEYWORD] at byte 13 | expected: UPPERCASE keyword | expected UPPERCASE keyword, found `where`
    branch-on-code => AGENT-ACTION: keyword not in closed core, suggest UPPERCASE

[empty_string] input=""
    code.as_str() = "UNEXPECTED_EOF"
    at (byte span) = 0
    expected set   = ["more input"]
    message        = "unexpected end of input"
    Display        = [UNEXPECTED_EOF] at byte 0 | expected: more input | unexpected end of input
    branch-on-code => AGENT-ACTION: input incomplete, request continuation
```

Full invalid set and their machine codes:
- `lowercase keyword` → `UNKNOWN_KEYWORD` @13
- `missing |>` → `UNEXPECTED_TOKEN` @10
- `unknown op (SHUFFLE)` → `UNEXPECTED_TOKEN` @13
- `empty string` → `UNEXPECTED_EOF` @0
- `missing SELECT cols` → `UNEXPECTED_EOF` @19

Verified machine-readable fields on `ParseError`:
- `at: usize` — byte span/offset (points at the offending token; e.g. 13 = start of `where`).
- `code: ParseErrorCode` — `#[non_exhaustive]` enum with a stable `as_str()`
  (`UNEXPECTED_TOKEN` / `UNEXPECTED_EOF` / `UNKNOWN_KEYWORD`). A consumer can `match`
  on the variant or compare the string.
- `expected: Vec<String>` — token-level expected-set drawn from the frozen
  `qfs_lang::Keyword` vocabulary.
- `message: String` — human-facing prose, plus a `Display` that renders all four
  fields. The type is `Clone + PartialEq + Eq` and carries NO winnow type
  (owned-DTO / no-vendor-leak guard G6).

### Item 3 — Production `qfs-parser` is panic-free on bad input — PASS

With `RUST_BACKTRACE=1`, none of the invalid or adversarial inputs panicked or printed
a Rust backtrace; the harness ran to its `"finished without panicking"` marker.
Adversarial results:
- `very_long` (`FROM ` + `a.`×5000 + `b`) → `Ok` (parsed, ops=0), no stack overflow.
- `unicode` (`FROM 日本語 |> WHERE 件名 = '請求書'`) → `Err [UNEXPECTED_TOKEN] at 5` (clean).
- `only_whitespace` → `Err [UNEXPECTED_EOF] at 8`.
- `embedded_newlines` → `Ok` (multispace handles `\n`; ops=2).
- `null_ish` (embedded NUL) → `Err [UNEXPECTED_TOKEN] at 5`.
- `emoji` → `Err [UNEXPECTED_TOKEN] at 5`.

UTF-8 byte spans are well-formed (no slicing panic on multibyte input — `grammar.rs`
guards with `input.get(at..)` / `peek_word` length clamp).

### Item 4 — CI wasm32 cross-check — ABSENT (documentation/forward-reference gap)

The Architect flagged that the ADR cites a wasm32 CI step. Reading
`.github/workflows/ci.yml`: there is **NO active `wasm32-unknown-unknown` build step**.
The wasm32 job exists ONLY as a commented-out placeholder (lines 79-86), labelled
"DEFERRED per t01 ... a parked placeholder so the matrix slot is visibly reserved".

However, two documents assert it as present fact:
- `docs/adr/0001-parser-library.md` line 95-96: the wasm32 build of `qfs-parser`
  "is validated **in CI only**".
- `crates/parser/src/lib.rs` line 24: "The `wasm32-unknown-unknown` build is
  validated in CI".

**Verdict: ABSENT.** This is a forward-reference / wording gap (claims "validated in
CI" when the step is commented out), NOT a build blocker — wasm32 is explicitly
deferred out of scope per t01/t02 and trip assumptions A2/A4. The structural basis for
the wasm claim is independently sound: `cargo tree -p qfs-parser` shows only
`qfs-lang` + `winnow`, and winnow pulls **zero** transitive deps (no `stacker`/`psm`),
so the crate is genuinely wasm-friendly even with the step parked.

## Concern + constructive proposal (Critical Review Policy)

**Concern (business/traceability):** A downstream stakeholder or AI agent reading the
ADR/lib docs will believe wasm32 is continuously gated when it is not. The claim is
aspirational but phrased in the present tense, which erodes the traceability the
closed-core design depends on.

**Proposal (business outcome framed):** Align the two docs with CI reality with a
one-line wording change — change "is validated in CI" to "is **deferred**; CI carries
a parked wasm32 placeholder (ci.yml lines 79-86) to be enabled when the target lands in
E1". Cost: a doc edit, zero code. Benefit: the wasm32 promise becomes a tracked,
honest forward-reference instead of a claim that fails on inspection — preserving the
auditability that is the selling point of the closed-core/open-registry positioning.
This is a documentation follow-up, not a t02 acceptance blocker.

## Observation (non-blocking) — task literal vs E0 grammar

The task's literal valid example `FROM /mail |> WHERE id |> SELECT subject` does NOT
match the E0-subset grammar: a leading `/` is not a valid path ident char, and bare
`WHERE id` lacks the required comparison (`<path> <op> <literal>`). The parser handled
it correctly and safely — returned `Err [UNEXPECTED_TOKEN] at byte 5`, no panic — so it
is a spec-example-vs-grammar mismatch, not a defect. I substituted a grammar-conformant
valid input (`FROM mail |> WHERE subject LIKE 'invoice' |> SELECT subject`) to exercise
the `Ok(Stmt)` path. If `/`-prefixed mailbox paths are intended for E0, that is a
grammar-scope question for the Architect/Constructor; for t02 (skeleton) it is out of scope.

## Overall verdict

**E2E approved.**

- Item 1 (spike comparison): PASS
- Item 2 (parse_statement structured-error contract): PASS
- Item 3 (panic-free on adversarial input): PASS
- Item 4 (CI wasm32 step): ABSENT — recorded as a documentation/forward-reference gap,
  NOT a blocker (wasm32 is deferred per scope; the structural wasm-friendliness claim
  holds via the zero-transitive-deps tree).

The t02 parser skeleton exposes a clean, owned, machine-branchable `ParseError`
(span + code + expected-set + message), is panic-free on adversarial input, and the
winnow-vs-chumsky spike runs and reproduces its golden corpus. Approved with the single
non-blocking documentation follow-up above (wasm32-in-CI wording).
