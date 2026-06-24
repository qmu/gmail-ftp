---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [DB, Infrastructure]
effort:
commit_hash: 23a2a35
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t14-pushdown-planner-and-local-engine.md]
---

# Driver: SQL databases (postgres / mysql / sqlite)

## Overview

This ticket implements the **relational/table archetype** (RFD §5 table, archetype 2) for
real SQL databases, mounted at `/sql/<conn>/<schema>/<table>`. It is the canonical pushdown
driver: a single-source pipeline against one connection must collapse into **one native SQL
statement** (RFD §6 "one SQL query per DB"), and a single-DB `COMMIT` must be a **real ACID
transaction** (RFD §6 transactions). It realizes the driver contract (RFD §5) — namespace +
per-node schema from `information_schema`/catalog to power `DESCRIBE`, capabilities, pushdown,
and DML via the universal verbs `INSERT/UPSERT/UPDATE/REMOVE/SELECT` (RFD §3 "path is the
type"). It honors the purity invariant (RFD §3): write verbs build a `Plan`; only `COMMIT`
opens the transaction and executes.

This is the proving ground for federation (T14): correct per-dialect native-SQL generation
here is what lets the planner push `WHERE/SELECT/JOIN/AGGREGATE/ORDER/LIMIT` down and only
combine cross-source remainders locally.

## Scope

In scope:
- Three dialects behind one driver: **postgres, mysql, sqlite** (connection string selects).
- Catalog introspection (columns, types, PK/unique, FK) → owned schema DTOs → `DESCRIBE`.
- Per-dialect **native-SQL emitter** for the pushdownable pipeline prefix (T14 hands a
  `PushdownPlan` subtree; this driver renders dialect SQL + bound params).
- DML lowering: `INSERT INTO`, `UPSERT INTO` (ON CONFLICT / ON DUPLICATE KEY / INSERT OR
  REPLACE), `UPDATE … WHERE`, `REMOVE` (DELETE), `RETURNING` where supported.
- Single-connection ACID `COMMIT` (BEGIN → effects → COMMIT/ROLLBACK).
- Capability declaration per node (tables: full CRUD; views: SELECT-only).
- `AS OF` (RFD §4 `@version`) **declared unsupported** with a structured error unless the
  table is a temporal table — wired as capability, generation deferred.

Out of scope (deferred):
- Cross-source orchestration / 2-phase semantics → effect-plan runtime ticket (E2) + T14.
- Local combine engine (DuckDB vs own evaluator) → T14.
- D1 (Cloudflare) as a SQL target → its own E4 ticket (shares the dialect=sqlite emitter but
  uses native binding, not a socket client).
- Schema DDL (CREATE TABLE / migrations) — not a universal verb; not in this ticket.
- Connection-pool tuning policy / secrets backend → E5 auth/credentials.

## Key components

New crate `qfs-driver-sql` (thin clients only — RFD §9 "no heavy vendor SDKs"; `sqlx` with
`postgres,mysql,sqlite` features as the thin async client, or `tokio-postgres`+`mysql_async`
+`rusqlite` if `sqlx` footprint is rejected by the spike).

- `enum Dialect { Postgres, Mysql, Sqlite }` — drives quoting, placeholders (`$n` vs `?`),
  upsert syntax, type mapping.
- `struct SqlDriver { conns: HashMap<String, ConnHandle> }` implementing the `Driver` trait
  (T13). Vendor row/column types **never leak** past this boundary (RFD §9 owned DTOs).
- `impl Driver for SqlDriver` surface:
  - `fn describe(&self, node: &PathNode) -> Result<NodeSchema>` — from cached catalog.
  - `fn capabilities(&self, node: &PathNode) -> Capabilities` — table vs view.
  - `fn pushdown(&self, plan: &PipelineIr) -> PushdownSplit` — declares the renderable prefix.
  - `fn execute_read(&self, q: &PushdownPlan) -> Result<RowStream>` — render + run SELECT.
  - `fn build_effects(&self, w: &WriteStmt) -> Result<Vec<Effect>>` — DML → effect nodes
    (pure; no I/O).
  - `fn commit(&self, effects: &[Effect]) -> Result<EffectReport>` — single-conn ACID txn.
- `mod catalog` — `fn introspect(&Dialect, &mut Conn) -> Catalog` querying
  `information_schema.columns/key_column_usage/table_constraints` (pg/mysql) and
  `pragma_table_info`/`pragma_foreign_key_list` (sqlite); maps to owned `ColumnDef { name,
  ty: ColType, nullable, pk, unique }` where `ColType` is qfs's type enum (RFD §4), not a
  vendor type.
- `mod emit` — `fn render_select(&Dialect, &PushdownPlan) -> SqlText` and
  `fn render_dml(&Dialect, &Effect) -> SqlText`; returns `(sql, Vec<Param>)` with bound
  params only (no string interpolation of values).
- `mod conn` — connection registry keyed by `<conn>`; credentials pulled from the encrypted
  store (E5), never logged; each effect leg carries timeout (RFD §6/§10).

## Implementation steps

1. Spike the client choice (`sqlx` vs hand-rolled trio); confirm `wasm32` exclusion is OK
   (these clients are native-only — sqlite/D1-on-wasm is the separate D1 ticket).
2. Define `Dialect` + the `qfs-driver-sql` crate skeleton; register the `/sql` mount.
3. `conn` module: parse connection URI, resolve secret ref, open + health-check connection.
4. `catalog::introspect` for all three dialects → owned `Catalog`; cache per connection.
5. Wire `describe` + `capabilities` off the catalog; reject unknown table/column at parse
   time with structured errors (RFD §5 "important for AI").
6. `emit::render_select`: quoting, placeholders, `WHERE/SELECT/EXTEND/AGGREGATE/GROUP/ORDER/
   LIMIT/DISTINCT` + single-source `JOIN` within one connection; param binding.
7. Implement `pushdown` to report exactly the prefix `emit` can render; remainder returns to
   the local engine (T14).
8. `execute_read` → stream rows back as qfs DTO rows (typed via catalog).
9. `build_effects`: lower `INSERT/UPSERT/UPDATE/REMOVE/RETURNING` into `Effect` nodes
   (pure, with `irreversible` flag set for DELETE/UPDATE without retry-safe key).
10. `emit::render_dml` per dialect, incl. upsert variants and `RETURNING` (pg/sqlite native;
    mysql emulate via `LAST_INSERT_ID`/secondary select where needed).
11. `commit`: BEGIN; apply effects in DAG order on one connection; COMMIT or ROLLBACK on any
    error; emit `EffectReport` rows into the audit ledger (RFD §6/§10).
12. Golden tests: pipeline IR → expected dialect SQL string (per dialect) — no live DB.
13. Integration tests against embedded sqlite + (gated) ephemeral pg/mysql containers.

## Considerations

- **Hard part — dialect divergence.** Upsert, `RETURNING`, identifier quoting, boolean/JSON/
  timestamp type mapping, and placeholder syntax all differ. Resolve by making `Dialect` the
  single decision point and covering each branch with golden SQL tests; never emit SQL by
  string-formatting values — **always bind params** (injection + correctness).
- **Pushdown fidelity vs. semantics.** SQL `NULL`/collation/ordering semantics must match
  qfs's pure-query semantics, or pushed-down results differ from locally-combined ones. Where
  a construct can't be faithfully rendered, the driver must *decline* it in `pushdown` and let
  the local engine handle it — declining is correct, mis-rendering is a bug.
- **ACID boundary.** Single connection = single transaction is the guarantee (RFD §6). Effects
  in one `COMMIT` against one `<conn>` share one txn; spanning two connections is **not** this
  ticket's job — surface a structured "cross-source, use orchestrated commit" error.
- **Least-privilege & secrets (RFD §10).** Connection creds come from the encrypted store,
  redacted in all logs/errors; the driver runs with whatever DB-side grants the operator gave
  — document recommending read-only roles for read-only mounts. Capability gating rejects
  writes to view nodes before any I/O.
- **Idempotency/recovery (RFD §6).** `UPSERT` is the retry-safe path for at-least-once server
  triggers; `commit` is all-or-nothing per connection (rollback on failure), and every applied
  effect is written to the audit ledger for reconstruction.
- **Observability.** Per-leg timeout, bounded retries on transient connection errors, circuit
  breaker per `<conn>`; structured logs of rendered SQL (params redacted) and row counts.
- **Coding standards / structure.** Owned DTOs only at the boundary; `Dialect` match is
  exhaustive (no `_ =>` fallthrough that silently mis-renders); modules `catalog/emit/conn`
  kept separate from the `Driver` impl.

## Acceptance criteria

- `cargo build` and `cargo clippy -- -D warnings` green for `qfs-driver-sql`.
- Golden tests: for a fixed pipeline IR, the emitted SQL string matches the expected text for
  **each** of postgres/mysql/sqlite (no live DB needed) — plan/SQL assertions are the primary
  gate.
- `pushdown` returns the maximal renderable prefix and correctly declines un-renderable
  constructs (asserted: declined remainder routed to local engine).
- `DESCRIBE /sql/<conn>/<schema>/<table>` returns catalog-derived columns/types/PK matching a
  fixture schema (sqlite, in-process).
- Capability test: `INSERT/UPDATE/REMOVE` into a view node is rejected at parse time with a
  structured error; `SELECT` allowed.
- ACID test (embedded sqlite + gated pg/mysql): a multi-effect `COMMIT` that fails mid-way
  leaves zero rows changed (rollback verified); a successful `COMMIT` is atomic and writes the
  audit ledger entries.
- Upsert test: `UPSERT INTO` renders ON CONFLICT (pg/sqlite) / ON DUPLICATE KEY (mysql) and is
  retry-safe (running twice yields one row).
- No secret material appears in any emitted log line or error (assertion over redaction).
- All value-bearing SQL uses bound parameters (no value interpolation) — verified by emitter
  unit tests.
