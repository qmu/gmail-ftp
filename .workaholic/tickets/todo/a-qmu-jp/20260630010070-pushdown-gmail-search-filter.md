---
created_at: 2026-06-30T01:01:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: 2h
commit_hash:
category: Changed
depends_on: []
---

# Pushdown: let Gmail do its own search filtering (wire `build_query` + LIMIT)

Roadmap "Near-term backlog — push more work into the source": Gmail's search filters are still done
by qfs after fetching everything. A complete `WHERE → q=` translator already exists and is
unit-tested — it's just not called by the read facet.

## Confirmed gap

- Gmail declares `PushdownProfile::Partial { where_: true, limit: true, … }`, and
  `crates/driver-gmail/src/query.rs:62 build_query` turns a predicate into a Gmail `q=` string
  (tested).
- But `crates/driver-gmail/src/read.rs:25 read_rows(..., _predicate: Option<&Predicate>)` takes the
  predicate as **`_`-unused** ("predicate … stays a local residual", lines 18-20) and scopes by label
  only with a fixed `READ_CAP` — it ignores both the pushed filter and the pushed LIMIT. So filtering
  happens in qfs after fetching, exactly as the roadmap says.

(Under-applying is *correct* — `crates/exec/src/read.rs:21-24` lets qfs re-apply for correctness —
this ticket makes it *fast*.)

## Plan

1. In `crates/driver-gmail/src/read.rs`, call `build_query(predicate)` and send the resulting `q=` to
   the Gmail list call; thread the planner's LIMIT (`PushedQuery.limit`,
   `crates/pushdown/src/physical.rs:21`) into the page size / cap instead of the fixed `READ_CAP`.
2. Keep the local residual for any predicate fragment Gmail's `q=` can't express (correctness).
3. Tests: a `where`-filtered `/mail/<label>` read issues the expected `q=` and respects `limit`.

## Key files

- `crates/driver-gmail/src/read.rs:25` (wire the predicate + LIMIT), `crates/driver-gmail/src/query.rs:62`
  (`build_query`, exists), `crates/pushdown/src/physical.rs:21` (`PushedQuery`),
  `crates/exec/src/read.rs:21` (under-apply contract).

## Considerations

- Don't flip `project`/`order` in the declared profile here — only `where_`/`limit` get honored.
- Bump the patch in `crates/qfs/Cargo.toml`. Sibling of the SQL pushdown ticket (`20260630010080`).
