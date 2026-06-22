# ADR 0002 — Local combine engine: embedded DuckDB vs. in-house evaluator

- **Status**: Accepted (locked)
- **Date**: 2026-06-23
- **Deciders**: cfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t14 — pushdown planner + local combine engine decision
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §1 (single binary + wasm32 Workers target), §6 (pushdown
  federation, "Local combine engine decision: embed DuckDB vs. own relational evaluator
  — footprint vs. build cost — open"), §9 (Implementation: no heavy vendor SDKs / owned
  DTOs), ADR-0001 (the analogous winnow-vs-chumsky decision and its dependency-weight /
  wasm-buildability criteria).

## Decision

**cfs ships an in-house relational evaluator — `cfs_engine::MiniEvaluator` — as the local
combine engine, behind the `CombineEngine` trait.** Embedded DuckDB (the `duckdb` crate)
is **not** taken. The trait is the reversibility seam: a `DuckDbEngine` could be added
later behind a non-default cargo feature without touching any caller, exactly as ADR-0001
kept the parser choice reversible behind an owned `ParseError`.

The combine engine runs **only the cross-source residual** a pushdown plan leaves local
(filter, project, hash-join, set ops, group/aggregate, sort, limit, EXPAND). The heavy
lifting — large `WHERE`/`GROUP BY`/`JOIN` over a single backend — is pushed down to that
backend by `cfs_pushdown::partition_by_source`, so the local engine only ever sees a small
relational subset.

## Context

RFD §6 explicitly flagged this as open: "embed DuckDB vs. own relational evaluator
(footprint vs. build cost — open)". DuckDB is an excellent embedded analytical engine and
would give us a complete, battle-tested SQL executor essentially for free. The question is
whether its cost fits cfs's deployment envelope (RFD §1/§9): **a single static binary that
also targets `wasm32-unknown-unknown` for Cloudflare Workers, with no heavy vendor SDKs.**

This is the same decision shape as ADR-0001 (winnow vs chumsky): a capable dependency vs.
the RFD §9 "lean, wasm-clean, owned-boundary" default. As there, we did not decide on faith
— we measured the deployment-relevant facts and weighed them against what the engine
actually needs to do, which the pushdown planner (the other half of t14) deliberately
minimizes.

## Comparison (evidence, not opinion)

### 1. wasm32-unknown-unknown buildability — the decisive criterion

cfs's Workers face (RFD §1) requires the engine to compile to `wasm32-unknown-unknown`.

- **DuckDB**: the `duckdb` crate links the DuckDB C++ engine via a C/C++ build
  (`libduckdb-sys`, a `cc`/bindgen native build). `wasm32-unknown-unknown` has no libc /
  C++ runtime and no filesystem; DuckDB's amalgamation does not target it. DuckDB's own
  wasm story is **`duckdb-wasm`, a separate Emscripten (`wasm32-emscripten`) artifact
  loaded as a JS/WASM module** — not a Rust `wasm32-unknown-unknown` static link, and not
  usable inside a `wasm32-unknown-unknown` Worker binary. So embedding DuckDB would
  **forfeit the Workers target** or fork the engine per platform (native = DuckDB, wasm =
  something else) — exactly the divergence RFD §9 warns against (cf. ADR-0001's rejection
  of chumsky's C-built `psm`/`stacker` as "wasm-hostile").
- **`MiniEvaluator`**: pure Rust over owned `cfs_types` values. Its **entire dependency
  closure is the serde family** (`cargo tree -p cfs-engine`: `cfs-pushdown` → `cfs-driver`
  → `cfs-plan` → `cfs-types` → `serde`/`serde_json` + `thiserror`). No `cc`, no bindgen,
  no native build, no threads, no `std::fs`, no sockets — it is wasm-clean by construction
  (the `#![cfg_attr]` lint policy + the workspace's `unsafe_code = forbid` hold).

### 2. Binary / static footprint

- **DuckDB**: the DuckDB engine is large. The host's DuckDB CLI binary is **~49 MB**
  (`/usr/local/bin/duckdb`, measured); `libduckdb-sys` statically links a comparable C++
  core into the host binary. That dwarfs every other cfs dependency combined and
  contradicts RFD §1/§9's "single, *lean* binary".
- **`MiniEvaluator`**: a few hundred lines of Rust compiled into the existing serde-only
  closure — negligible added footprint.

### 3. Build cost / supply chain

- **DuckDB**: pulls a C++ toolchain requirement into CI and cross-compile (a `cc`/clang
  build per target), lengthening clean builds and adding a non-Rust supply-chain surface
  — the same class of cost ADR-0001 weighed against chumsky's `psm` C build.
- **`MiniEvaluator`**: pure-Rust, no extra toolchain; clean build is the existing serde
  compile.

### 4. Capability vs. what the residual actually needs

- DuckDB's strength is **heavy** analytical SQL. But in cfs that heavy work is **pushed
  down** to the source backend (`PushdownProfile::Full`/`Partial` → a native `ScanNode`).
  The local engine only ever combines the **cross-source residual**: hash-join two
  pushed-down legs, union/except/intersect, a residual filter/projection/aggregate a
  `None`-pushdown source could not run, EXPAND. That is a small, closed operator set —
  precisely what `MiniEvaluator` implements. Paying DuckDB's footprint to run a residual
  that is, by design, small is the wrong trade.
- Honest counter-point: a hand-rolled engine must be *correct*. We mitigate this with the
  **differential property** — `cfs_pushdown` guarantees the split is semantically total
  (residual-over-scans == naive all-local), and `cfs-engine`'s tests assert it on
  in-memory fixtures. A future cost-based optimizer or a heavy local-analytics need could
  reopen this behind the `CombineEngine` feature seam without a rewrite.

## Consequences

- **Positive**: the default build stays a single lean binary, `wasm32-unknown-unknown`
  stays reachable (the Workers target is not forfeited), no C++ toolchain enters CI, and
  the engine has zero non-serde dependencies. The `CombineEngine` trait keeps the door
  open for an optional `DuckDbEngine` (native-only, behind a non-default feature) if a
  heavy *local* analytical workload ever justifies it.
- **Negative / accepted**: we own the correctness of a relational evaluator. Scope is
  bounded to the residual operator set; the differential property test is the guard. A
  full SQL executor (window functions, complex join orders, statistics) is explicitly out
  of scope — those either push down to a real backend (E4 drivers) or wait for a future
  cost-model ticket.
- **Reversibility**: because no DuckDB type ever crosses the `CombineEngine` /
  `PushedQuery` boundary (owned DTOs only, RFD §9), swapping the engine impl is a
  feature-gated addition, not a refactor.

## Notes on local DuckDB availability

DuckDB **is** installed on the build host (`/usr/local/bin/duckdb`, v1.4.4), which is what
let us measure the ~49 MB footprint directly rather than guess. Its availability as a CLI
does not change the decision: the question is embedding it *in the cfs binary / Worker*,
where the wasm32 and footprint facts above are dispositive. The host CLI remains usable as
an out-of-band analysis tool, not as the engine cfs ships.
