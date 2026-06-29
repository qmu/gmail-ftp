---
created_at: 2026-06-30T01:01:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 4h
commit_hash:
category: Changed
depends_on: []
---

# Pushdown: grow SQL's `QuerySpec` to push projection / ORDER BY / LIMIT into the database

Roadmap "Near-term backlog — push more work into the source": a SQL database runs the `WHERE`, but
column selection, sorting, and limits are still done by qfs after fetching everything — even though a
SQL backend could trivially do all of them.

## Confirmed gap

- The SQL driver declares only `where_` and its read `QuerySpec` lowers only WHERE; projection /
  ORDER BY / LIMIT / aggregate / group_by / distinct / join are **not threaded through** —
  self-documented in `crates/driver-sql/src/tests.rs:863`
  (`pushdown_declares_where_only_until_queryspec_grows`).
- The planner already computes and carries the rest: `crates/pushdown/src/planner.rs:241-261`
  (`supports_project/limit/order` gates) and `crates/pushdown/src/physical.rs:21 PushedQuery
  { filter, project, limit, order }`. It's the SQL read facet that drops them.

## Plan

1. Grow the SQL read `QuerySpec` (around `crates/driver-sql/src/lib.rs:114` and the read path) to
   carry `project`, `order`, and `limit` from `PushedQuery`, and emit them into the generated SQL
   (`SELECT <cols> … ORDER BY … LIMIT …`).
2. Flip the declared `PushdownProfile` flags (`project`, `order`, `limit`) to `true` **only after**
   the read honors them; keep the local residual for anything not pushed (correctness via
   `crates/exec/src/read.rs:21`).
3. Update / retire the `pushdown_declares_where_only_until_queryspec_grows` test to assert the new
   pushed SQL.

## Key files

- `crates/driver-sql/src/lib.rs:114` (profile + QuerySpec), `crates/driver-sql/src/tests.rs:863`,
  `crates/pushdown/src/{planner.rs:241,physical.rs:21}`.

## Considerations

- Stay conservative on aggregate/group_by/distinct/join — those are riskier (the local residual must
  still reconstruct correct results); this ticket is scoped to project/order/limit. Note follow-ups.
- Bump the patch in `crates/qfs/Cargo.toml`. Sibling of the Gmail pushdown ticket (`20260630010070`).
