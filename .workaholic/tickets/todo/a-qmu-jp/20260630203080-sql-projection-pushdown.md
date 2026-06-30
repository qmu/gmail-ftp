---
created_at: 2026-06-30T20:31:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash:
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
