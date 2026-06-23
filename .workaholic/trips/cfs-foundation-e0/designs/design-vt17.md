# Design vt17 — Driver: SQL databases (postgres / mysql / sqlite)

Author: Constructor
Status: approved
Reviewed-by: (coding-phase ticket design; lead-directed)

## Content

### Scope and inventory

New runtime-leaf crate `cfs-driver-sql` implementing the t13 `cfs_driver::Driver` contract
for the **relational/table archetype** over real SQL databases, mounted at `/sql`. One driver,
three dialects (postgres / mysql / sqlite) behind a `Dialect` decision point and a pluggable
`SqlBackend` connection abstraction. Real tests run against an in-process `rusqlite` (bundled,
vendored C SQLite — native aarch64, no external server); postgres/mysql share the **same
compiled-SQL path** and are covered by per-dialect golden SQL string tests (no live server).

Modules (each separate from the `Driver` impl, per ticket "Coding standards / structure"):
- `dialect` — `enum Dialect { Postgres, Mysql, Sqlite }`: quoting, placeholders (`$n` vs `?`),
  upsert syntax, type mapping. Exhaustive matches, no `_ =>` fallthrough.
- `path` — `SqlPath` parse of `/sql/<conn>/<schema>/<table>` (and `/sql`, `/sql/<conn>`).
- `catalog` — owned `Catalog` / `ColumnDef { name, ty: ColumnType, nullable, pk, unique }`;
  `introspect` per dialect; maps SQL types → `cfs_types::ColumnType`; `describe_schema()`.
- `emit` — `render_select(&Dialect, &SelectPlan) -> (String, Vec<Param>)` and
  `render_dml(&Dialect, &DmlOp) -> (String, Vec<Param>)`. **Bound params only**; never a value
  string-interpolated. `Param` is an owned value enum mirroring `cfs_types::Value` scalars.
- `compile` — cfs query (projection / `WHERE` / `ORDER BY` / `LIMIT`) → `SelectPlan` + a
  **truthful residual** `Option<Predicate>` (the t20/t21 lesson; SQL is the lucky exact case).
- `conn` — `SqlBackend` trait (the connection abstraction); `SqliteBackend` (rusqlite) the real
  impl; `ConnHandle` registry keyed by `<conn>`; credentials via `cfs-secrets::Secret`.
- `applier` — `SqlApplier`: lowers DML effect nodes to parameterized statements and applies them
  inside one ACID transaction (BEGIN → effects → COMMIT/ROLLBACK).
- `error` — `SqlError` taxonomy: structured, secret-free, AI-consumable; `From<SqlError> for
  EffectError`.

### Implementation approach

- **Driver surface** (`SqlDriver`): `mount() = "/sql"`; per-node `describe` returns
  `RelationalTable` + the cached catalog schema; `capabilities` = table → full CRUD
  `{SELECT,INSERT,UPSERT,UPDATE,REMOVE}`, view → `{SELECT}` only (capability gating rejects a
  write to a view at the parse-time gate before any I/O); `pushdown` = `Partial { where_, project,
  limit, order, aggregate, group_by, distinct, join }` all true (SQL is a full backend — but
  declared as `Partial` with every flag set so the planner can still query by intent, matching the
  GA precedent of an honest declaration; `Full` is also acceptable, but `Partial`-all-true lets a
  future un-renderable construct be turned off one flag at a time).
- **Query → parameterized SQL** (`emit::render_select`): `SELECT <cols> FROM <quoted table>
  WHERE <predicate> ORDER BY <...> LIMIT <n>`. Identifiers are dialect-quoted; **every value is a
  bound placeholder** (`?` for sqlite/mysql, `$1..$n` for postgres) pushed into `Vec<Param>`.
- **Truthful residual** (`compile`): a `WHERE` conjunct compiles to an exactly-equivalent SQL
  predicate → residual dropped (`Cmp` with `= <> < > <= >=`, `IN`, `BETWEEN`, `AND` of
  compilables). A predicate cfs cannot faithfully render to SQL with identical semantics is
  **kept as residual** (the engine re-filters): `LIKE` (cfs glob vs SQL `LIKE` differ), `~`
  (regex — not portable across the three dialects), `OR`/`NOT` mixing a residual child. Never
  wrong rows.
- **DML** (`build_effects` + `emit::render_dml`): `INSERT INTO`, `UPSERT INTO`
  (`ON CONFLICT DO UPDATE` pg/sqlite, `ON DUPLICATE KEY UPDATE` mysql), `UPDATE … WHERE`,
  `REMOVE` (`DELETE … WHERE`). DELETE/UPDATE without a retry-safe key set `irreversible`.
- **Transactions** (`SqlApplier`): single-connection = single transaction. A multi-effect commit
  applies in order on one connection; any error rolls back (zero rows changed); success commits
  atomically. Cross-connection is rejected with a structured "cross-source" error.
- **Secrets**: connection string / password pulled from `cfs-secrets` as a `Secret`, exposed only
  at connect; **never** logged, never in a `SqlError`, never in a DTO.

### Quality strategy (internal tests, all against in-process sqlite or pure golden)

1. Introspection: create tables → `describe` returns catalog columns/types/PK (sqlite, in-proc).
2. SELECT with WHERE/ORDER/LIMIT → parameterized SQL → correct rows over a temp sqlite DB.
3. INSERT/UPDATE/DELETE effects change exactly the right rows.
4. **Injection safety**: a value `'; DROP TABLE t; --` is bound as a parameter (table survives,
   the literal lands as data) — proven against a real sqlite DB.
5. Capability gating: write to a view rejected structurally; SELECT allowed.
6. Transaction: a mid-way failing multi-effect commit rolls back (zero rows changed); a clean
   commit is atomic.
7. End-to-end: a plan over `/sql` runs through the runtime interpreter via `PlanApplierBridge`.
8. Secret safety: a planted credential never appears in any `SqlError`/log surface.
9. Golden SQL: a fixed query renders the expected per-dialect SQL (pg `$n`, mysql/sqlite `?`,
   each upsert variant) — no live DB.

### Delivery plan / dep wiring

- Append `cfs-driver-sql` to the `runtime_consumers_allowed` allowlist in
  `crates/cmd/tests/dep_direction.rs` (a one-line reviewable signal; the generic leaf check
  guarantees it stays safe — nothing depends back onto it).
- Deps: `cfs-driver`, `cfs-plan`, `cfs-types`, `cfs-runtime`, `cfs-secrets`, `serde`, `thiserror`,
  `tracing`; dev: `rusqlite` (bundled), `tokio`. `rusqlite` is the **real** test backend only —
  the production SQL path is dialect-rendered text + a `SqlBackend` seam, so the crate's own
  compile/emit logic is vendor-free and the postgres/mysql paths need no live server.

### Risk assessment

- **Dialect divergence** (upsert / placeholders / quoting): centralized in `Dialect`; covered by
  per-dialect golden tests. Mitigated.
- **Pushdown fidelity**: decline (keep residual) on any non-exact construct — correctness over
  completeness. Mitigated by the truthful-residual rule + tests.
- **Secret leak**: `Secret` redaction + secret-free `SqlError` + a planted-canary test. Mitigated.
- **Spine/tokio confinement**: runtime leaf; allowlist + generic leaf check keep it green.

## Review Notes

Concern (engineering): bundling SQLite via `rusqlite`'s `bundled` feature pulls a C compile into
dev builds. Trade-off accepted because it is **dev-only** (the production path is text + the
`SqlBackend` seam), gives a real ACID/injection test without an external server (project-local,
system-safety `system_changes_authorized: false` honored), and matches the ticket's "use sqlite
for real tests" directive. Proposal if rejected later: feature-gate the rusqlite tests behind a
`sqlite-tests` feature so a minimal CI can skip the C build while the golden SQL tests still run.
