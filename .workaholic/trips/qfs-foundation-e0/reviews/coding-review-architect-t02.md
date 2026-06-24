# Coding Review (Architect) — t02 parser-library spike, ADR, and skeleton crate

- **Author**: Architect (Neutral / structural bridge, translation fidelity)
- **Phase / Step**: coding / review-and-testing
- **Reviewed commit**: `eb341f3`
- **Mode**: Analytical review only (no cargo / no test execution — per Architect QA domain)
- **Artifacts reviewed**:
  - `docs/adr/0001-parser-library.md`
  - `crates/parser/src/{lib.rs,error.rs,ast.rs,grammar.rs}` + `crates/parser/Cargo.toml`
  - `spikes/parser-spike/` (Cargo.toml, src/{lib,ast,winnow_spike,chumsky_spike}.rs, tests/comparison.rs, tests/golden/errors.txt)
  - root `Cargo.toml`, `crates/lang/src/keywords.rs`
  - `plan.md` A4, t02 ticket

## Decision

**Approve with minor suggestions.**

The t02 work is structurally sound. winnow is genuinely confined behind the owned
`ParseError` and a crate-private `grammar` module; no winnow/chumsky type leaks from
`qfs-parser`'s public surface; the ADR is evidence-driven rather than a restatement of the
RFD §9 default; the spike is correctly quarantined; and the skeleton's AST/`Stmt` shape is
a faithful, growable subset of the frozen `qfs-lang` vocabulary. The minor suggestions
below are observations and small fidelity tightenings, not structural defects.

---

## 1. G6 fidelity — vendor confinement and the owned error contract

**Verdict: PASS — no vendor leak.** Confirmed by reading the public surface, not by trusting the claim.

- `crates/parser/src/lib.rs` declares `pub mod ast; mod error; mod grammar;` — `grammar`
  (the only module that `use`s winnow) and `error` are **non-`pub`**. The structural
  guarantee is the module privacy, exactly as the ADR's Consequences section asserts.
- The entire public re-export list is owned: `ast::{CmpOp, Expr, Literal, Path, PipeOp,
  Stmt}`, `error::{ParseError, ParseErrorCode}`, and the `qfs_lang` re-exports. I traced
  every one of these types — none transitively names a `winnow` type. `winnow` is imported
  only in `grammar.rs`, and `map_error` is the single boundary mapper: it consumes
  `winnow::error::ParseError<…, ContextError>` and returns the owned `crate::ParseError`.
  The winnow error type is `&`-borrowed inside the function and never stored.
- `parse_statement(&str) -> Result<Stmt, ParseError>` is the stable signature the ticket
  required (acceptance criterion C5 direction `qfs-core → qfs-parser`, no cycle).

**`ParseError` shape vs the RFD §5 AI-agent contract: PASS.** `error.rs` carries exactly
`at: usize` (byte span), `code: ParseErrorCode` (machine-readable, with a stable `as_str()`
emitting `UNEXPECTED_TOKEN` / `UNEXPECTED_EOF` / `UNKNOWN_KEYWORD`), `expected: Vec<String>`
(the expected-set), plus a human `message`. It is `Clone + PartialEq + Eq` and implements
`std::error::Error` + `Display`. This is a machine-readable, comparable, serialisable-by-hand
DTO — well-suited to the §5 structured-error path. `ParseErrorCode` and `ParseError` are both
`#[non_exhaustive]`, so E1 can add finer codes (capability-rejected) without a breaking change.
The `pub(crate) fn new(...)` constructor keeps construction inside the boundary mapper, which
is the right encapsulation.

**Observation 1a (minor, structural-fidelity).** The `no_vendor_type_in_public_api` test is
a *behavioural* pin (it constructs an error and checks `Clone`/`Eq`/`Display` shape), not a
*structural* compile-time guard. It cannot fail if a future refactor makes `error` or
`grammar` `pub` or adds a `From<winnow::…>` impl on the public `ParseError`. The real guard
today is module privacy — which is correct and sufficient for E0 — but the test's name
over-promises relative to what it checks. *Proposal:* either rename it to
`parse_error_is_owned_and_displayable` (honest about scope), or in E1 add a `compile_fail`
doctest / trybuild case proving a `winnow::error::ContextError` cannot be named from a
downstream crate. Not blocking for E0.

**Observation 1b (minor, error semantics).** `map_error` classifies `UnknownKeyword` purely
by "the remaining input starts with an ASCII-lowercase char." That is a heuristic, not a
keyword-set check: a lowercase *path segment* appearing where a keyword is expected would be
reported as `UNKNOWN_KEYWORD` even though it is not keyword-shaped, and an unknown *UPPERCASE*
verb (`SHUFFLE`) is reported as `UNEXPECTED_TOKEN`, not `UNKNOWN_KEYWORD`. This is acceptable
for the E0 subset (the golden corpus shows the classification is stable and useful), but it
is worth recording that the `UnknownKeyword` code is currently approximate. *Proposal:* E1,
which has the full `qfs_lang::KEYWORDS` set available, should classify by membership/case
against that frozen set rather than by first-char case. Note as an E1 carry-over.

## 2. ADR soundness — evidence vs restatement, reversibility, recovery honesty

**Verdict: PASS — evidence-driven, not a §9 restatement.**

- The ADR does **not** merely repeat the §9 default. It builds a shared-AST head-to-head,
  commits a golden error corpus (`spikes/parser-spike/tests/golden/errors.txt`), and a
  cross-parser AST-equality test, then scores on the ticket's criteria. The "genuinely hard
  call" section directly engages the *only* RFD-sanctioned override reason (chumsky recovery
  for the §5 path) and argues it down with three concrete points. This is the right shape for
  a decision record: it earns the §9 default rather than assuming it.
- The headline machineability claim is **substantiated by the corpus**: for
  `lowercase_keyword`, winnow yields `[UNKNOWN_KEYWORD] … expected: UPPERCASE keyword` vs
  chumsky's char-level `expected: 'W', 'S'`; for `unknown_op`, winnow gives a token-level
  expected-set vs chumsky's `expected: 'E'`. The "token-level beats char-level for an AI
  consumer" argument is visible in the evidence, not asserted.

**Reversibility story: REAL.** The owned `ParseError` + owned AST + private `grammar` module
genuinely bound a future swap. A library change requires (1) rewriting `grammar.rs` against
the new library and (2) re-mapping its error into `ParseError` — `parse_statement`'s signature
and every public type are untouched, so `qfs-core` and all E1+ callers are insulated. I
confirmed there is no public type whose definition references winnow, so the blast radius is
structurally confined to one module. The ADR's Reversibility section accurately describes this.

**Recovery trade-off: HONESTLY represented.** The ADR states plainly that neither library
surfaces multi-errors out of the box and that chumsky recovery needs explicit
`.recover_with(...)`. I verified this against the spike: `chumsky_spike::parse_all_errors`
exists and is wired into the golden generator, and `chumsky-recovery-count: 1` appears for
**every** corpus case in `errors.txt`. The claim is backed by committed evidence, and the
"tie (neither free)" cell is fair. The decision does not overstate winnow or understate
chumsky's headline strength.

**Observation 2a (minor, evidence integrity).** The ADR's quantitative claims — compile
times (~1.34 s vs ~4.70 s), rlib sizes (~2.99 MB vs ~6.83 MB), and the chumsky transitive
set (`hashbrown`, `stacker`, `psm`, `libc`, `unicode-*`) — are stated in prose but are **not**
reproduced by any committed artifact (unlike the error corpus, which is locked by a golden
test). These were measured during the spike and are not re-derivable from the repo. For an
ADR this is acceptable (the numbers are a tiebreaker, and the decisive evidence is the locked
corpus), but the timing/size figures are unfalsifiable from the committed tree. *Proposal:* a
one-line note in the ADR that these figures are point-in-time spike measurements (not CI-
locked), or — better, low cost — commit the `cargo tree -p parser-spike` output as a small
evidence file. Non-blocking; the decision does not rest on these numbers.

**Observation 2b (minor, wasm honesty).** The wasm32 section is correctly marked CI-only /
deferred and ties to assumption A2/R5 (target not installed; `rustup target add` is a
forbidden toolchain mutation). The reasoning is sound and the deferral is honest. The one
residual risk is that the wasm32 build is asserted "validated in CI" — but CI for this
workspace is itself an E0 t01 deliverable. *Proposal:* confirm (Constructor/Planner domain)
that the t01 CI matrix actually contains a `cargo build -p qfs-parser --target
wasm32-unknown-unknown` step, so this ADR claim is not a forward reference to CI that does
not yet run that target. Verification belongs to the testing roles, not this analytical review.

## 3. Spike isolation — quarantine integrity and dep-graph confinement

**Verdict: PASS — winnow/chumsky stay out of the production graph except `qfs-parser → winnow`.**

- `spikes/parser-spike/Cargo.toml` has `publish = false` and is the **only** crate depending
  on `chumsky`. `qfs-parser/Cargo.toml` depends on `winnow` (and `qfs-lang`) and **not** on
  `parser-spike` — I confirmed there is no `parser-spike` path dependency anywhere in
  `crates/`. So the production dep graph is exactly `qfs-parser → winnow` (+ `qfs-lang`);
  chumsky is reachable only from the throwaway spike.
- The lint relaxation is correctly scoped: `#![allow(clippy::unwrap_used, expect_used, panic)]`
  lives in the spike's `lib.rs` and `comparison.rs` inner attributes only. `grammar.rs`
  explicitly does **not** relax the workspace `unwrap/expect/panic = deny` policy, and I
  verified it is panic-free at the boundary: `map_error` uses `input.get(at..).unwrap_or("")`
  (not `&input[at..]`, which the spike uses) and `peek_word` uses `.unwrap_or(s.len())` +
  `.get(..).unwrap_or(s)`. This is a real fidelity improvement of the production code over the
  spike — the spike's `&input[at..]` slice could panic on a non-char-boundary offset; the
  production mapper cannot. Good.
- Loser banner: PASS. `chumsky_spike.rs` carries an explicit "THROWAWAY SPIKE — NOT
  PRODUCTION CODE (the RETAINED LOSER, see ADR A3)" banner pointing at the ADR and warning
  against wiring it in. The winnow spike carries a parallel banner. The crate `description`
  and `lib.rs` header both repeat "THROWAWAY / NOT production / `qfs-parser` does NOT depend
  on it." The A3 requirement is satisfied unambiguously.

**Observation 3a (minor, "does adding `spikes/*` risk shipping spike code?").** Structurally,
no: `publish = false` keeps `parser-spike` off crates.io, and the single-binary build
(`qfs` / the server) depends on `qfs-core`-side crates, not on `parser-spike`, so the spike
is not in any shipped artifact. The one real consequence of the `members = ["crates/*",
"spikes/*"]` glob is that **a bare `cargo build` / `cargo test` / `cargo clippy` at the
workspace root now compiles and lints the spike**, pulling chumsky + `psm` (a C build) into
the *developer's* build and CI time — exactly the cost the ADR penalised chumsky for. That is
intended (the spike must build/fmt/clippy with the rest), but it means the chumsky/psm C
toolchain dependency is now a standing CI/dev-build requirement for as long as the spike lives
in the workspace. *Proposal:* since the ADR is the durable artifact and the spike "may rot,"
add a short-lived E1 carry-over to **delete `spikes/parser-spike` once E1's real grammar
lands** (or move it behind a `--workspace --exclude` / a non-default workspace setup), so the
wasm-hostile C dependency does not linger in CI indefinitely. Non-blocking for E0.

## 4. E1 readiness — does the real grammar grow in place without restructuring?

**Verdict: PASS — the skeleton is a faithful, growable subset; no restructuring needed for E1.**

- The grammar will grow **inside** `crates/parser/src/grammar.rs` (private) and the owned
  `crates/parser/src/ast.rs`. Adding EXTEND/SET/AGGREGATE/JOIN/effect verbs/DDL means adding
  `PipeOp` variants and combinators — the `Stmt { from, ops: Vec<PipeOp> }` shape already
  models "a FROM source plus a chain of `|>` operations," which is exactly the RFD §2 pipe
  pipeline. No public-signature change is forced; `parse_statement` stays stable.
- **Frozen-vocabulary alignment: PASS with one nuance.** `grammar.rs` sources `FROM`,
  `WHERE`, `SELECT` from `qfs_lang::Keyword::{From,Where,Select}.text()` (the frozen RFD §3
  enum), so the closed core has one home (boundary B6). The AST `CmpOp` subset (`Eq, Ne, Lt,
  Gt, Le, Ge, Like`) is a strict subset of `qfs_lang::OPERATORS`, and the keyword golden/
  freeze test in `keywords.rs` locks the 38-keyword / 15-operator counts — so the parser
  cannot invent a keyword outside §3 without the freeze test catching the `qfs-lang` change.
- The ADR/A4 claim that "keyword surface text is sourced from `qfs_lang::Keyword`, not
  hand-typed" is **mostly** true but slightly overstated — see Observation 4a.

**Observation 4a (minor, the one real fidelity gap I found).** Three tokens in `grammar.rs`
are hand-typed string literals rather than sourced from `qfs-lang`:
- `"AND"` in `expr()` (line 129) and again in `expected_tokens()` (line 70),
- `"LIKE"` in `cmp_op()` (line 107),
- `"|>"` in `statement()` (line 156) and `expected_tokens()` (line 71).

`AND`, `LIKE`, and `|>` are **operators** (they live in `qfs_lang::OPERATORS`, not in the
`Keyword` enum), so there is no `Keyword::And.text()` to call — the "sourced from the frozen
`Keyword` set" claim is literally accurate (only keywords are sourced), but the *spirit* of
"the closed core has exactly one home" is not fully met: these three operator surface strings
are duplicated between `qfs_lang::OPERATORS` and `grammar.rs`, and could drift. For the E0
subset this is harmless (operators are frozen and the set is tiny). *Proposal:* in E1, give
`qfs-lang` a typed operator accessor (an `Operator` enum mirroring `Keyword`, or named consts
like `qfs_lang::op::AND`) and have the grammar reference those, so operators get the same
single-home guarantee keywords already have. Record as an E1 carry-over. This is the only
place where the "one home" fidelity claim is weaker than advertised, and it is minor.

**Observation 4b (minor, AST relocation note — informational).** `ast.rs` documents that the
statement-level AST is "slated to live in `qfs-lang` in E1" and that E0 keeps a local copy.
That is a reasonable E0 decision, but it means E1 faces a *relocation* (move `Stmt`/`PipeOp`/
`Expr` from `qfs-parser` to `qfs-lang`) that **will** touch the `pub use` surface of
`qfs-parser` — i.e. the "no public signature changes on a library swap" reversibility
guarantee does not extend to the planned AST move (a different axis of change). This is not a
defect — the two are orthogonal (library swap vs AST home) — but the team should be aware that
E1's AST relocation is a deliberate, reviewed public-surface change, not a silent one. The
`#[non_exhaustive]` on the error types does not cover the AST structs/enums; if E1 wants the
AST move to be non-breaking it should consider `#[non_exhaustive]` on `Stmt`/`PipeOp`/`Expr`
or plan the relocation as an explicit semver step. Informational; no E0 action.

## Cross-artifact coherence

The ADR, the plan A4 record, the skeleton crate, and the spike tell **one consistent story**:
winnow won on machineability + footprint + maintenance; recovery was not decisive; winnow is
confined behind owned types; the spike is throwaway evidence. The plan A4 decisions match the
ADR's content (location, wasm deferral, loser banner, recovery honesty) with no contradiction.
The ticket's acceptance criteria map cleanly onto delivered artifacts (shared AST + cross-parser
equality test + committed golden corpus + ADR with table + owned `ParseError` + no-vendor-leak
audit + keyword constants with one home). Translation fidelity from RFD §2/§3/§5/§9 → ticket →
artifacts is high.

## Summary of suggestions (all minor, none blocking E0)

1. (1a) Rename `no_vendor_type_in_public_api` to reflect it is a behavioural pin, or add a
   `compile_fail`/trybuild structural guard in E1.
2. (1b) E1: classify `UnknownKeyword` against the frozen `KEYWORDS` set, not by first-char case.
3. (2a) Mark the ADR's compile-time/size figures as point-in-time spike measurements, or commit
   a `cargo tree` evidence file.
4. (2b) Testing roles: confirm t01 CI actually runs the `wasm32` `qfs-parser` build the ADR cites.
5. (3a) Add an E1 carry-over to delete/exclude `spikes/parser-spike` after E1 lands, so the
   chumsky/`psm` C dependency does not linger in CI.
6. (4a) E1: give `qfs-lang` a typed operator surface (`AND`/`LIKE`/`|>`) so operators get the
   same single-home guarantee keywords have (the one real, minor fidelity gap).
7. (4b) Informational: plan E1's AST relocation to `qfs-lang` as an explicit reviewed
   public-surface change; consider `#[non_exhaustive]` on the AST types.

## Review Notes

Analytical review only — no cargo/clippy/test execution (Architect QA domain). Internal test
results (qfs-parser 7 tests, spike 3 tests green) and E2E/CLI validation are the Constructor's
and Planner's domains respectively and are not re-verified here.
