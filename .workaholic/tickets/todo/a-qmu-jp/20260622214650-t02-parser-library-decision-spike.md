---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260622214650-t01-rust-workspace-single-binary-scaffold.md]
---

# Parser library decision spike (winnow / chumsky / combine)

## Overview

`cfs` exposes every external service through **one uniform pipe-SQL DSL**
(`FROM <path> |> <op> |> <op> …`, UPPERCASE keywords) — see RFD 0001 §2 (Pipe-SQL),
§3 (closed core + three open registries), and §9 (Implementation: parser research
2026-06-22). The DSL parser is the front door of the whole engine: it turns DSL text
into the AST sum types that §9 names as the reason Rust was chosen over Go, and every
downstream epic (E1 language core, E2 effect-plan, E7 server DDL) builds on it.

This ticket is the **decision spike** that locks the parser library before any real
grammar work begins. Research as of 2026-06-22 (RFD §9): **winnow** is the default
(actively maintained — commits this week; function-based, no macros; fastest; fixes the
nom/combine gripes), **chumsky** is the fallback if DSL parse-error recovery proves
decisive (now on Codeberg, GitHub archived), and **combine** is deprioritized (sporadic
maintenance, ~4.5 months idle). We do not take §9 on faith: we spike a tiny
`FROM |> WHERE |> SELECT` grammar in both winnow and chumsky, compare error quality and
ergonomics on real cfs constructs, then **record an ADR locking the choice**. The output
is a decision + a thin parser-skeleton crate, not the production grammar.

## Scope

In scope:
- A throwaway-but-committed spike: parse `FROM <path> |> WHERE <expr> |> SELECT <cols>`
  in **winnow** and in **chumsky**, into a shared minimal AST.
- Side-by-side comparison on: error message quality (especially on the AI-critical
  "structured error" path of RFD §5), multi-error recovery, ergonomics of `|>`-chained
  pipelines, UPPERCASE-keyword handling, span/position reporting, and wasm32 build size.
- An **ADR** (`docs/adr/0001-parser-library.md`) recording the criteria, the spike
  evidence, and the locked choice.
- A thin `cfs-parser` crate skeleton wired to the chosen library: module layout,
  the `ParseError` type shape, and a `parse_statement(&str) -> Result<Stmt, ParseError>`
  signature — enough for ticket t-(E1) to build the real grammar on.

Out of scope (deferred):
- The full closed-core grammar (all keywords in RFD §3), codecs, effect verbs, server
  DDL — deferred to **E1 Language core** tickets.
- AST → effect-plan lowering and the `Plan` type — deferred to **E2** tickets.
- Capability gating / parse-time rejection of unsupported verbs (RFD §5) beyond a single
  illustrative example — deferred to **E1/E4**.
- Registry resolution (paths / functions / codecs) — deferred to **E1**.

## Key components

New crate `cfs-parser` (workspace member from t01), spike code under `cfs-parser/spikes/`:

- `spikes/winnow_spike.rs` and `spikes/chumsky_spike.rs` — two implementations of the
  same grammar, each exposing `fn parse(input: &str) -> Result<SpikeStmt, SpikeError>`.
- Shared minimal AST (sum types per RFD §9), e.g.:
  ```rust
  pub struct SpikeStmt { pub from: Path, pub ops: Vec<PipeOp> }
  pub enum PipeOp { Where(Expr), Select(Vec<Column>) }
  pub enum Expr { Cmp { lhs: Path, op: CmpOp, rhs: Literal }, And(Box<Expr>, Box<Expr>) }
  pub enum CmpOp { Eq, Ne, Lt, Gt, Le, Ge, Like }   // subset of RFD §3 operators
  ```
- `src/error.rs` — the production-facing `ParseError` (owned, no library types leaking
  past the crate boundary, mirroring the "owned DTOs / no vendor leaks" rule of RFD §9):
  byte/char span, expected-set, and a machine-readable code for the AI structured-error
  path.
- `src/lib.rs` — locked public surface: `pub fn parse_statement(src: &str)
  -> Result<Stmt, ParseError>` (a stub returning `unimplemented!`/minimal parse until E1),
  plus a re-export of the AST module. Closed-core keyword constants live here so the
  frozen keyword set (RFD §3) has one home.
- `docs/adr/0001-parser-library.md` — the locked decision record.

## Implementation steps

1. Add `cfs-parser` as a workspace member (depends on t01 scaffold); add `winnow` and
   `chumsky` as **dev-dependencies** only (spikes), not yet a runtime dep.
2. Define the shared `SpikeStmt`/`PipeOp`/`Expr` AST in `spikes/common.rs`.
3. Implement `winnow_spike.rs`: `FROM` path, `|>`-separated ops, `WHERE` with
   `AND`/comparison operators, `SELECT` column list; UPPERCASE keywords; capture spans.
4. Implement `chumsky_spike.rs` with the same grammar, enabling its error-recovery to
   surface **multiple** errors per input.
5. Write a shared corpus of ~10 inputs: valid pipelines plus deliberately broken ones
   (missing `|>`, lowercase keyword, dangling `WHERE`, unterminated string, unknown op).
6. Golden-snapshot the rendered error for each broken input from both libraries
   (`insta` or committed `.golden` files); these are the comparison evidence.
7. Benchmark parse throughput on a representative pipeline and record `wasm32` build-size
   delta for each library (RFD §9 footprint concern; §1 wasm32 target).
8. Score winnow vs chumsky against the criteria; write `docs/adr/0001-parser-library.md`
   with the table, the golden evidence, and the **locked choice** (default winnow unless
   error recovery is decisive per RFD §9).
9. Wire `cfs-parser/src/` to the chosen library: promote it from dev-dep to dep, add
   `error.rs` + the `parse_statement` signature + closed-core keyword constants. Delete
   the losing spike (or move both under `spikes/` clearly marked non-production).
10. `cargo build`, `cargo clippy -- -D warnings`, `cargo test`, and a `wasm32` build all green.

## Considerations

- **AI structured errors are the load-bearing requirement** (RFD §5: unsupported ops
  rejected "at parse time (structured error — important for AI)"). The comparison must
  weight error *machineability* (span + expected-set + code), not just human prose. This
  is the genuinely hard call and the only reason to pick chumsky over the default winnow —
  resolve it with the committed golden corpus, not opinion.
- **No vendor type leaks** (RFD §9): whichever library wins, its error/parser types must
  not appear in `cfs-parser`'s public API — wrap into the owned `ParseError`. This keeps
  the choice reversible (we can swap libraries later without breaking E1+).
- **Frozen keyword set** (RFD §3): define the closed-core keywords as constants in one
  place now so the spike and the future grammar share a single source of truth; the spike
  only exercises a subset but must not invent keywords outside §3.
- **Purity / effects-as-data**: the parser produces AST only; it performs no I/O and
  constructs no effects (effect-plan is E2). Keep `cfs-parser` free of any network, fs, or
  credential dependency — least-privilege at the crate level (RFD §10), and it makes the
  parser fully unit-testable with **no live creds**.
- **wasm32 footprint** (RFD §1, §9): both libraries are pure-Rust and wasm-friendly;
  still record the size delta since the binary ships to Workers — a tiebreaker if error
  quality is close.
- **Coding standards / directory structure**: spike code is quarantined under `spikes/`
  and clearly non-production; the surviving public surface is minimal and documented. The
  ADR is the durable artifact — the spike binaries may rot, the decision must not.
- **Reversibility/recovery**: this ticket commits to a library but the wrapper boundary
  is the escape hatch; document in the ADR the exact swap cost if the choice is revisited.

## Acceptance criteria

- `cargo build`, `cargo clippy -- -D warnings`, and `cargo test` are green; a
  `cargo build --target wasm32-unknown-unknown -p cfs-parser` succeeds.
- Both `winnow_spike` and `chumsky_spike` parse every valid input in the corpus into the
  identical shared AST (asserted by tests comparing the two outputs).
- Golden tests exist for every broken-input case in both libraries and are committed; the
  rendered errors are reproducible (snapshot-stable).
- `docs/adr/0001-parser-library.md` exists, states the criteria, includes the comparison
  table + benchmark/size numbers, and records a single **locked** library choice with
  rationale tied back to RFD §9 (and §5 if recovery is the deciding factor).
- `cfs-parser` exposes `parse_statement(&str) -> Result<Stmt, ParseError>` and an owned
  `ParseError` (span + expected-set + machine code); no library-internal types appear in
  its public API (asserted by a doc/test or a `pub use` audit).
- Closed-core keyword constants (RFD §3 frozen set) live in `cfs-parser` and are
  referenced by the parser rather than hard-coded inline.
- No network/filesystem/credential dependency is introduced; all tests run with no live
  credentials.
