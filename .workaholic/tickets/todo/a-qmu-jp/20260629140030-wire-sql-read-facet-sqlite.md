---
created_at: 2026-06-29T14:00:30+09:00
author: a@qmu.jp
type: enhancement
layer: [DB, Infrastructure]
effort:
commit_hash:
category: Added
depends_on: []
---

# T4 — Wire `/sql` read facet (SQLite hermetic; Postgres via live connection)

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 2. Multi-day — see sub-tasks.

## Overview

`/sql/<conn>/<table> |> where … |> select …` errors `unknown source 'sql'` and `describe /sql/…` →
`unknown_mount`: `driver-sql` has `conn.rs`/`applier.rs` but **no read facet**, and is mounted only when
configured. This wires a SQL read facet, with **SQLite as the hermetic, test-covered backend** (the
docs' `/sql/pg/...` Postgres path works against a live connection but is not hermetic).

## Ground truth (verified 2026-06-29)

- `driver-sql` exports only `sql_apply_driver` (`crates/qfs/src/commit.rs:273-279`,
  mount `crates/qfs/src/shell.rs:215-217`); no read facet. Read lookup failure: `crates/qfs/src/exec.rs:60-66`.
- `driver-sql/src/{conn.rs,path.rs,applier.rs}` present; pushdown profile exists in `crates/pushdown`.

## Sub-tasks (each a ≤4h commit)

1. **Read layer** — implement a read path in `driver-sql` (`read.rs`) that executes the planned
   SELECT/where/limit pushdown against a connection and returns rows; start with SQLite.
2. **Registration** — register the sql read facet in `crates/qfs/src/shell.rs` when a connection is
   configured; ensure `describe /sql/<conn>/<table>` returns the column schema.
3. **Tests** — hermetic SQLite fixture DB: read, filter-pushdown, aggregate; assert rows + that the
   filter pushes down (the docs' "pushes filters into the database" headline).
4. **Postgres note** — keep Postgres reachable via a real connection; document it as non-hermetic.

## Key files

- `crates/driver-sql/` (new read facet), `crates/qfs/src/{shell.rs,read_facets.rs}`, `crates/pushdown/`.

## Considerations

- Makes `databases.md`, `cross-service.md`, and the `concepts.md`/`index.md` SQL↔GitHub join's SQL leg
  true (Phase 5). The cross-service join also needs T3/T6 for the other leg.
- The docs say `/sql/pg/orders`; either ship a sqlite example path or make the conn alias explicit.
