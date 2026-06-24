# Coding Review (Architect) — t03 lexer + t04 grammar/AST/governance

Author: Architect
Status: complete
Phase: coding / review-and-testing
Scope: analytical review only (no cargo/test execution) — catch-up for the two
tickets implemented without the Architect gate.
Commits reviewed: `0ec2168` (t03 lexer), `f96bfee` (t04 grammar/AST/governance).
Model checked against: `models/model-v1.md` (guards G1, G2, G3, G5, G6; the acyclic
dependency spine §4) and the RFD §3 frozen list (`.workaholic/RFDs/0001-qfs-architecture.md`).

---

## Files read

- t03: `crates/lang/src/{lib,span,error,token,keywords,lex}.rs`, `crates/lang/tests/lex.rs`,
  `crates/lang/Cargo.toml`.
- t04: `crates/parser/src/{lib,error,ast,grammar}.rs`, `crates/parser/src/tests.rs`,
  `crates/parser/tests/golden_ast.rs`, `crates/parser/Cargo.toml`, `Cargo.lock` (winnow/serde rows).
- Cross-checks: `crates/cmd/Cargo.toml`, `crates/core/Cargo.toml`, workspace `Cargo.toml`,
  RFD §3 frozen-keyword list.

---

## Ticket t03 — Lexer / tokenizer

### Decision: Approve with observations

t03 faithfully realises the model's closed-core / single-keyword-home thesis and the
ticket's acceptance criteria. The structural verdicts:

**G1 (single keyword source) — HELD, and strengthened.** The frozen set lives in exactly
one place, `crates/lang/src/keywords.rs`, as both the `Keyword` enum and the `KEYWORDS`
fixture. I cross-checked the fixture byte-for-byte against RFD §3 (lines 53–61): all 38
reserved words present, none extra. The freeze is enforced by three tests — `KEYWORDS.len()
== 38`, an order-independent multiset equality of `Keyword::text()` vs `KEYWORDS`, and a
duplicate guard — so the enum and the flat fixture cannot drift apart. The operator set is
separately frozen at 15 (`|>` + 14), also matching §3. There is no second keyword table
anywhere; `SizeUnit`/`LitType`/`literal_word` recognise *non*-keyword words (units,
`DATE/TIME/TIMESTAMP`, `TRUE/FALSE/NULL`), which RFD §3 correctly does NOT list as reserved
keywords, so these are not a competing keyword source.

**Zero-dep / wasm-clean (B7) — HELD.** `crates/lang/Cargo.toml` has an empty
`[dependencies]`. The lexer is a hand-written `char_indices` cursor — no winnow, no
combinator crate, no `std::fs`/threads/sockets. This is exactly the model's reason for the
lexer living in `qfs-lang` rather than `qfs-parser`: it keeps the closed core dependency-free
and makes G6 (no winnow leak) trivially true on the lang side.

**Token taxonomy + spans — COMPLETE.** Every `Token` variant from the ticket sketch is
present; spans are `u32` byte ranges with a round-trip helper in the tests asserting
`&src[span.range()]` slices back exactly and that spans are non-overlapping/ordered. The
documented lexing decisions (path-vs-division, space-set size literals, bounded typed-literal
lookahead, `@version` binding to the preceding segment, multi-word keywords as adjacent
tokens) are all implemented and match the module doc-comment.

**Panic-free — HELD.** Arbitrary-UTF-8 fuzz corpus plus a 2000-codepoint sweep; integer
overflow maps to `BadNumber` (`raw.parse::<i64>()` error), not a panic; `as u32` span casts
`unwrap_or(u32::MAX)` rather than panic. Char-boundary safety holds because the cursor only
ever advances whole `char`s, so every span lands on a boundary.

### Observations (no revision required)

1. **O1 — `byte_len` truncation is silent, not just saturating (minor robustness).**
   `push`/`err` cast offsets with `u32::try_from(..).unwrap_or(u32::MAX)`. For the intended
   domain (a single statement) this never triggers, and the doc-comment says so. But if a
   >4 GiB source were ever fed in, multiple tokens would collapse to `u32::MAX` and the
   round-trip invariant would silently break rather than error. *Proposal (defer to E1):* if
   the parser ever accepts multi-statement / large inputs, add a single up-front
   `src.len() <= u32::MAX as usize` guard returning a `LexError`, so the invariant fails loud.
   Not a t03 defect — the domain assumption is documented.

2. **O2 — `LexError` carries no `LexErrorKind::BadNumber`-with-cause detail.** The structured
   error names the kind but not whether it was overflow vs malformed. The AI-legibility
   contract (RFD §5) is satisfied at the kind level today; finer codes are explicitly reserved
   by `#[non_exhaustive]`. No action now.

3. **O3 — `peek_size_unit` requires a space (`25 MB` yes, `25MB` no).** This is the
   ticket's intended behaviour (a column literally named `MB` is unaffected) and is tested.
   Recording it because it is a deliberate, load-bearing grammar decision the parser inherits.

---

## Ticket t04 — Grammar + AST + closed-core governance

### Decision: Approve with observations

t04 is a faithful structural projection of RFD §2.2/§3/§4/§8 and clears every guard the model
made load-bearing for the parser boundary.

**G6 (no winnow leak) — HELD, audited.** `winnow` types appear ONLY inside the
crate-private `mod grammar` (`crates/parser/src/lib.rs` declares `mod grammar;` un-`pub`).
I grepped the whole `crates/parser/src/` tree: every `winnow` occurrence outside `grammar.rs`
is a doc-comment, never a type. The public surface is the owned `Statement` (+ AST sum types)
and owned `ParseError`/`ParseErrorCode`; `parse_statement(&str) -> Result<Statement,
ParseError>` is the stable, reversible boundary. The boundary mapper (`map_error` /
`lex_to_parse_error`) converts winnow's `ParseError<&[Spanned<Token>], ContextError>` and the
lexer's `LexError` into the owned type at the edge. `tests::no_vendor_type_in_public_api`
locks that the public error is clone/eq/displayable with no library type in scope. This is the
single most important fidelity guard for the parser crate and it is genuinely structural, not
aspirational.

**Closed-core grammar (variant counts locked) — HELD.** `Statement` (4) and `PipeOp` (18)
are deliberately NOT `#[non_exhaustive]`, and `tests::closed_core_variant_counts_are_locked`
mechanically pins both counts by constructing one of each. A later ticket adding a per-driver
`Send`/`Merge` variant would have to edit this test — a reviewed change-control event. The 18
`PipeOp` variants map one-to-one to RFD §3 query/transform keywords plus the three registry
seams (`Decode`/`Encode`/`Call`); there is no per-action variant. This is the structural
encoding of "new backend = zero keywords."

**Three open seams are the ONLY extension points — HELD.** Driver-specific behaviour can
enter the AST only as a *string* inside `PathExpr.segments` (path/mount registry),
`CallRef`/`FnRef` (procedure/function registry), or `Codec.fmt` (codec registry). The grammar
parses these as bare identifiers and never resolves them (`// shape only` comments throughout;
resolution explicitly deferred to E2). `Ident = String` reinforces that names are registry
concerns, not grammar.

**Governance rejections — HELD.** `classify()` distinguishes a lowercase keyword-shaped ident
(`UnknownKeyword`, "closed-core keywords are UPPERCASE") from a reserved keyword in identifier
position (`ReservedAsIdentifier`). `ident()` matches only `Token::Ident`, so a reserved word
can never satisfy an identifier slot — it falls through to a structured error. Both paths are
tested (`lowercase_keyword_rejected_as_unknown_keyword`,
`reserved_word_as_identifier_is_rejected`). `ParseError` always carries a non-empty `expected`
set (`expected_set()`), satisfying the RFD §5 AI-self-correction contract, and `Display`
never echoes literal values (`error_display_does_not_echo_string_literal_value`).

**serde placement — CORRECT per model.** `serde` is a dependency of `qfs-parser` only;
`qfs-lang/Cargo.toml` stays empty. The AST supplies its own `serialize_span` projection rather
than deriving `Serialize` on `qfs_lang::Span`, precisely so the zero-dep closed-core crate is
not infected with serde (documented in `ast.rs`). This is the right side of the boundary.

**Spine still acyclic — HELD.** t03/t04 touched neither `crates/cmd/Cargo.toml` nor
`crates/core/Cargo.toml`. `qfs-parser` depends on `qfs-lang` + `winnow` + `serde` only; it
does NOT depend on `qfs-core` (the reserved `qfs-core -> qfs-parser` edge is declared-but-not-
wired in core's Cargo.toml with the C5 note, correctly one-directional). `qfs-cmd` still
depends on `qfs-core` + `qfs-server` only (G5 intact — the `qfs-lang`/`qfs-parser` lines in
cmd's manifest are a "must NOT depend on" comment, not a dependency). No back-edge, no cycle.

### Will E5/E2 grow inside these AST seams without restructuring? — YES, with two caveats

The seams are shaped to absorb the next epics without touching the closed core:

- **E2 (effect-plan / name resolution / capability gating)** consumes the existing
  `CallRef`/`FnRef`/`Codec`/`PathExpr` string names and the `EffectVerb` Insert/Upsert
  distinction (preserved precisely so the runtime can pick a retry-safe verb, RFD §6). No new
  AST node is needed — resolution is a *pass over* this AST, not a change to it. The structured
  `ParseError` already reserves finer codes via `#[non_exhaustive]` (e.g. capability-rejected),
  so E2 adds codes without a breaking change.
- **E5 (capability/POLICY enforcement)** is enabled by `DdlKind::Policy` + the `target`
  desugar vector (`/server/policies/<name>`) already produced by the parser. Enforcement is
  downstream of the AST; the grammar correctly only validates *shape*.

Caveats — neither blocks approval:

- **C1 — `ServerDdl` clause grammar is permissive (order-independent loop).** `server_ddl`
  collects `ON/EVERY/AS/DO` in any order, each at most once, with no per-`DdlKind` validation
  (a `POLICY` may currently carry an `EVERY`, a `JOB` an `AS`). The ticket explicitly scopes
  this as "validate *shape*, desugaring lives downstream," so this is intended for E0/E1.
  *Proposal:* when E7 wires server-DDL desugaring, add a per-kind clause-legality check there
  (not in the grammar), keeping the grammar permissive and the validation in the semantic
  phase — consistent with the closed-core/open-seam split. Record this as a reserved seam now.

- **C2 — `EffectBody::Pipeline` vs `Source::Subquery` overlap for write sources.** An
  `INSERT INTO /x FROM ... |> ...` body reuses the full `pipeline` parser, which is correct and
  composes cleanly; just noting that E2 must treat the write-source pipeline and a read
  pipeline uniformly (they share the type), which the current shape already supports.

### Observations (no revision required)

1. **O4 — `raw_token_text` flattens a `Token::Path` operand with `/`-join and can absorb a
   literal value into `ON`/`EVERY`.** For `ON`/`EVERY` operands this is intentional (routes /
   intervals, not credentials, per the doc-comment) and the `found`-description secret-hygiene
   rule is preserved on the *error* path. No leak into diagnostics. Recording it because
   `raw_token_text` is the one place the parser stringifies a literal value rather than naming
   its kind; if a future DDL clause ever carries credential-bearing operands, this is the spot
   to revisit. Defer.

2. **O5 — `value_column_list` uses a manual stream save/restore (`let after_cols = *input;`
   … `*input = after_cols;`).** This is sound because winnow `&[T]` streams are `Copy` (cursor
   is a slice), and it is documented. It is the one hand-rolled backtrack in the grammar;
   correct, but worth a property test in E1 over `VALUES (a,b) (1,2)` vs `VALUES (1,2)` to lock
   the column-vs-row disambiguation against future combinator changes. The happy paths are
   covered by `insert_values_returning`. Defer.

3. **O6 — golden coverage is "pinned + determinism sweep," not byte-pinned for every corpus
   item.** `pinned_goldens` byte-locks 5 representative statements across all four families;
   the 25-item `CORPUS` is locked for parse-success + serialise-determinism only. This is a
   reasonable reviewability/coverage trade and explicitly justified in the test header. If a
   shape regression slipped between the 5 pinned cases it would only be caught structurally,
   not byte-for-byte — acceptable for E0, worth widening the pinned set as the AST stabilises.

---

## Cross-ticket coherence

t03 and t04 compose cleanly across the lexer→parser boundary the model drew: the lexer emits
`Vec<Spanned<Token>>` with byte spans; the grammar consumes that slice via winnow's `&[T]`
stream and re-spans each AST node from the token spans, so diagnostics round-trip to source
end-to-end. Multi-word keywords are split by the lexer (adjacent UPPERCASE idents) and
re-stitched by the parser (`group_by`/`order_by`/`insert_into`/`upsert_into`/
`materialized_view` via `word(..)` matchers) — composition is the parser's job, exactly as
the model and both tickets specify. The shared `Span`/`Spanned` primitives live once in
`qfs-lang` and flow through both crates without duplication.

## Summary

| Ticket | Decision | Load-bearing guards |
|---|---|---|
| t03 lexer | Approve with observations | G1 held (38-kw single source), B7 zero-dep/wasm-clean, spans round-trip, panic-free |
| t04 grammar/AST/governance | Approve with observations | G6 held (winnow confined to private `grammar`; owned `Statement`/`ParseError`), closed-core variant counts locked (4/18), 3 open seams string-only, serde in parser not lang, spine acyclic |

No structural defect found in either ticket; no revision requested. The recorded
observations/caveats (O1–O6, C1–C2) are forward seams for E1/E2/E7, not E0 corrections.
