---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash: 1a89bb6
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t10-interpreter-batch-parallel.md]
---

# Pushdown planner + local combine engine decision

## Overview

Implements the **pushdown federation** bullet of RFD §6 (and the federation half of
epic E3). A cfs pipeline (`FROM <path> |> <op> |> …`) may straddle several sources —
a Postgres table JOINed against git commit history, an S3 listing UNIONed with a Drive
listing. Today the interpreter (T10) would pull every leaf into the local engine and
evaluate naively. This ticket teaches the planner to **split the logical plan by source**:
collapse each maximal same-source subtree into a single native operation the driver runs
itself (one SQL query per DB, git plumbing for git, one list call per blob mount), and
evaluate only the **cross-source residual** locally.

It also resolves the open question RFD §6 flags explicitly — *"Local combine engine
decision: embed DuckDB vs. own relational evaluator (footprint vs. build cost — open)"* —
via a time-boxed spike and a written recommendation, then implements the chosen engine
behind a trait so the decision stays reversible.

Pushdown capability is sourced from the `Driver::pushdown` declaration (RFD §5) introduced
in T13; this ticket consumes that contract, it does not define it.

## Scope

In-scope:
- A **source-splitting planner pass** over the pure query AST (the `FROM/WHERE/SELECT/
  EXTEND/AGGREGATE/JOIN/UNION/EXCEPT/INTERSECT/EXPAND` subset only).
- A `Pushdown` negotiation: ask each driver how much of its subtree it can execute; emit a
  native `ScanNode` (opaque, driver-owned descriptor) for the accepted part and a local
  residual for the rest.
- A `CombineEngine` trait + the **local relational evaluator** chosen by the spike, covering
  the operators that can appear in a *cross-source residual* (filter, project, hash-join,
  set ops, group/aggregate, sort, limit, EXPAND).
- The **spike artifact**: a short benchmark comparing DuckDB-embed vs. hand-rolled evaluator
  on binary size, wasm32 buildability, and combine latency, plus the recommendation recorded
  in this ticket's PR description.
- Plan assertions: a `EXPLAIN`/plan-dump showing the source partitions and the residual.

Out-of-scope (deferred):
- Effect-plan (write) splitting and cross-source transactions / partial-failure recovery —
  owned by the effect-plan tickets (E2; cp = copy→verify→delete ledger). This ticket is
  **read/query side only**; effects stay opaque leaves here.
- Batch + auto-parallelize of independent native scans — that scheduling lives in T10; this
  ticket only *produces* the independent `ScanNode`s T10 will parallelize.
- Cost-based join ordering / statistics — emit a deterministic, rule-based plan now; a cost
  model is a future ticket.
- Materialized `VIEW` caching (RFD §8) — server-side, separate epic.
- Actual driver-native SQL generation per backend (the real Postgres dialect, git plumbing)
  lives in the driver tickets (E4); here drivers behind a test fake satisfy `Pushdown`.

## Key components

New crate `cfs-plan` (Domain) and `cfs-engine` (Infrastructure).

- `enum LogicalPlan` — sum type over the pure query operators (one variant per closed-core
  query keyword; no effect variants). Built from the AST by a lowering already owned upstream.
- `struct SourceId(pub Arc<str>)` — the mount/driver a subtree resolves to.
- Planner entry:
  ```rust
  pub fn partition_by_source(plan: LogicalPlan, reg: &DriverRegistry) -> PhysicalPlan;
  pub enum PhysicalPlan {
      Scan(ScanNode),                 // fully pushed to one source
      Combine { op: CombineOp, inputs: Vec<PhysicalPlan> }, // local residual
  }
  pub struct ScanNode { source: SourceId, pushed: PushedQuery, schema: Schema }
  ```
- `trait Pushdown` (declared in T13, consumed here):
  ```rust
  fn accept(&self, sub: &LogicalPlan) -> PushdownResult; // { accepted: PushedQuery, residual: Option<LogicalPlan> }
  ```
  `PushedQuery` is an **owned DTO** — an engine-side description of the work (predicates,
  projection, limit), never a vendor query object; the driver later translates it to SQL/
  plumbing inside its own boundary (no vendor type leak, RFD §9).
- `trait CombineEngine { fn execute(&self, plan: &PhysicalPlan, scans: ScanResults) -> RowStream; }`
  — the seam that keeps DuckDB-vs-own reversible. The chosen impl (`MiniEvaluator` or
  `DuckDbEngine`) sits behind it.
- `Schema` / `Row` / `Value` — reuse the data model from T13 (typed columns, `struct`/`array`,
  `@version`); the combine engine must round-trip nested values for `EXPAND` / `a.b.c`.
- `fn explain(&PhysicalPlan) -> String` — the plan-dump for golden tests.
- Capability gating: `partition_by_source` rejects at plan time (structured error) any
  subtree whose driver's `capabilities` deny an op, mirroring RFD §5 parse-time rejection.

## Implementation steps

1. Define `LogicalPlan` (pure-query variants only) and `PhysicalPlan`/`ScanNode` in `cfs-plan`.
2. Implement `partition_by_source`: post-order walk; tag each leaf with its `SourceId`; a node
   whose entire subtree shares one `SourceId` is a pushdown candidate.
3. For each candidate subtree, call `Pushdown::accept`; wrap the accepted part in `ScanNode`,
   re-graft any residual back as a local `Combine` input. JOIN/UNION/EXCEPT/INTERSECT across
   two different `SourceId`s always become local `Combine`s.
4. Define the `CombineOp` set the residual evaluator must support (filter, project, hash-join,
   set ops, group+aggregate, sort, limit, EXPAND).
5. **Spike** (time-boxed, on a branch): (a) `DuckDbEngine` over the `duckdb` crate — measure
   release binary delta and attempt a `wasm32-unknown-unknown` build; (b) `MiniEvaluator`
   hand-rolled over `RowStream`. Bench combine latency on a 2-source JOIN fixture.
6. Record the recommendation in the PR body (expected outcome: **own `MiniEvaluator`** — DuckDB
   cannot build to `wasm32-unknown-unknown` for Workers and adds large static footprint,
   contradicting RFD §9 "single binary / wasm32 / no heavy vendor SDKs"; we only need a small
   relational subset for *residuals*, since the heavy lifting is pushed down).
7. Implement the chosen engine behind `CombineEngine`; wire `partition_by_source` → engine into
   the interpreter so independent `ScanNode`s surface for T10's batcher.
8. Implement `explain()` and add golden plan-dump tests.

## Considerations

- **Hard part — pushdown boundary correctness.** A predicate referencing two sources must
  *not* be pushed to either; a single-source predicate over a JOIN's pushable side *should*.
  Resolve with a column-provenance check before offering a subtree to `accept`, and keep the
  residual semantically total (engine result == naive all-local result). Differential test:
  run every fixture both fully-local and partitioned, assert equal row sets.
- **Footprint / operation (RFD §9).** Keep `cfs-engine` dependency-light; gate any optional
  `DuckDbEngine` behind a non-default cargo feature so the default static + wasm build stays
  lean. CI must build `--target wasm32-unknown-unknown` to keep the Workers target honest.
- **Effects-as-data + purity (RFD §3).** This pass touches only the **pure** query side; it
  must never reorder, drop, or duplicate effect nodes — effects pass through as opaque leaves.
  Add a debug assertion that no `Plan` effect node enters `partition_by_source`.
- **Least-privilege & secrets.** The planner sees only owned DTOs and schemas, never tokens;
  `PushedQuery` carries no credentials. Capability/`POLICY` gating (RFD §10) is enforced before
  a scan is emitted, so an out-of-policy source fails at plan time, not at COMMIT.
- **Observability (RFD §6).** Each `ScanNode` carries the `SourceId` so per-leg timeouts,
  retries, and structured logs from T10 attribute correctly; `explain()` output doubles as the
  audit-friendly plan record.
- **Determinism.** Rule-based partitioning only — identical input plan ⇒ byte-identical
  `explain()` output, so golden tests are stable and AI agents get reproducible plans.
- **Coding standards.** Domain types (`LogicalPlan`, `PhysicalPlan`) in `cfs-plan` with no I/O;
  the evaluator + any DuckDB feature in `cfs-engine` (Infrastructure). No vendor types cross
  the `Pushdown`/`CombineEngine` traits.

## Acceptance criteria

- `cargo build`, `cargo build --target wasm32-unknown-unknown`, and `cargo clippy
  -- -D warnings` are green; the default feature set pulls in **no** DuckDB/cgo dependency.
- `partition_by_source` golden tests: a single-source pipeline lowers to exactly one
  `Scan` (zero local `Combine`); a cross-source JOIN/UNION lowers to `Combine` over two
  `Scan`s — asserted via `explain()` golden strings.
- Differential property test: for ≥20 generated pipelines, partitioned execution returns the
  same row set as forced all-local execution.
- A two-source predicate is provably not pushed to either side; a single-source predicate over
  a JOIN is pushed (asserted on the plan, not by hitting a live service).
- Capability/policy-denied subtree yields a structured plan-time error, never a partial scan.
- An out-of-policy or effect node entering the pure pass triggers the debug assertion (tested).
- The spike recommendation (DuckDB vs. own evaluator) is recorded in the PR with the binary-size
  and wasm32-buildability numbers backing it; the chosen engine is wired behind `CombineEngine`.
- No live credentials used in any test (driver fakes satisfy `Pushdown`).
