---
created_at: 2026-06-30T20:31:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash: 8268a35
category: Changed
depends_on: []
---

# Push column projection (and beyond) into the SQL backend (owner item #7)

## What's wanted

Deepen source-side pushdown. SQL already pushes `WHERE`/`ORDER BY`/`LIMIT` (commit `aebf67c`), but
**column projection** (and aggregates/group_by/joins) still run locally.

## The trap (why projection was deferred)

The SQL read facet uses the driver's full `describe` schema for the `RowBatch`; if projection is
pushed, rows carry only the projected columns → a schema/row mismatch. AND a pushed projection that
drops a column the **residual** predicate needs would break local re-filtering. So projection
pushdown must:

1. Use the planner's post-projection `scan.schema` (not the full describe schema) when projection is
   pushed (`crates/qfs/src/read_facets.rs::SqlReadDriver::scan`).
2. Ensure the projected columns ⊇ the residual predicate's columns (or not push projection when a
   residual references a non-projected column) — the compile (`crates/sql-core/src/compile.rs`)
   already lowers projection into `SelectPlan.projection`; the gating is the new work.
3. Flip `project: true` in `PushdownProfile` (`crates/driver-sql/src/lib.rs`) only after the facet
   honours it; update the self-doc test `pushdown_declares_where_order_limit_until_queryspec_grows`.

## Follow-ups (separate, riskier)

Aggregate / group_by / distinct / single-source JOIN pushdown (GA + cf already declare these; SQL
could). Keep each behind a flipped flag + correctness tests.

## Key files

- `crates/qfs/src/read_facets.rs`, `crates/sql-core/src/compile.rs`, `crates/driver-sql/src/{lib.rs,
  tests.rs}`, `crates/pushdown/src/planner.rs`.

## Considerations

- Same correctness discipline as the LIMIT guard (commit `aebf67c`): never return wrong rows; the
  engine re-applies the residual, so a pushed optimization must not strip a column the residual reads.

## Final Report

Development completed as planned — all three steps, plus the trap fully closed. Column projection now
pushes into the native SELECT, but `compile` EXPANDS the SELECT to keep every column the residual
predicate reads (`projected ⊇ residual columns`, via a new `residual_columns` collector with an
*exhaustive* match so a new `Predicate` variant can't silently drop a needed column), `execute_query`
returns the effective output `Schema`, and the facet narrows to the requested projection LAST —
after the residual re-filter has used the extra columns. `project: true` flipped in the
`PushdownProfile`; the self-doc test renamed/updated and the skill golden corpus realigned.

### Discovered Insights

- **Insight**: There are TWO residual layers, and only the COMPILE-level one (LIKE/regex/OR) can drop
  a projected column — and it is unknown at planning time. So the planner pushes projection
  optimistically (no local Project op), and the facet MUST deliver exactly the projection; the
  compile/facet split (over-select in compile, narrow in facet) is the only correct architecture
  because the planner cannot gate on a residual it hasn't computed.
  **Context**: `crates/qfs/src/read_facets.rs::SqlReadDriver::scan` must narrow to `scan.pushed.project`
  whenever it is `Some`, regardless of residual — a pushed projection leaves nothing else to do it.
- **Insight**: The direct `execute_query` unit tests bypass the planner and pass a projection inline,
  so they always exercised projection in the SELECT; production never did (the planner left
  `project:false` → `SELECT *`). Flipping the flag is what connects the planner to the long-present
  compile-side projection — the wiring gap was the planner→facet path, not the compiler.
  **Context**: When auditing pushdown, distinguish "compiler supports X" from "planner pushes X" —
  they were independently true/false here.
