# Coding Review — Architect — t17 (SQL databases driver)

- Reviewer: Architect (Neutral / structural bridge)
- Target: t17 — `qfs-driver-sql`, commit `43c5af1`
- Scope: analytical review only (no test execution). Base for t23 (D1 = sqlite-over-HTTP).
- Files read: `crates/driver-sql/src/{lib,dialect,emit,compile,catalog,conn,applier,error,path}.rs`,
  `crates/driver-sql/src/tests.rs`, `crates/cmd/tests/dep_direction.rs`, `crates/driver-sql/Cargo.toml`.

## Decision

**Approve with observations.**

No injection, secret-leak, or residual-truthfulness defect found. The dialect seam is clean
enough for t23 to reuse. Observations below are forward-looking (mostly t23/future-dialect) and
carry concrete proposals; none rises to "Request revision".

---

## 1. Injection safety (headline) — PASS

Audited the full path from value origin to SQL text. **Every value reaches the backend as a
bound `Param`, never as SQL text.** Confirmations:

- **Value carrier is a typed enum, not a string.** `emit::Param` (`Null/Bool/Int/Float/Text/Bytes`)
  is the only thing that crosses into the backend's `execute_read(sql, params)` /
  `commit_transaction(ops)`. The SQL string and the value vector are structurally separate types —
  a value cannot be in the `sql: String` because the lowering never puts it there.
- **WHERE rhs** (`render_where`): `Cmp`/`InList`/`Between` emit only `dialect.placeholder(n)`
  (`$n`/`?`); the value is pushed into `params` by index. No `format!` interpolates a value —
  every `format!` in `emit.rs` interpolates either a quoted identifier or a placeholder token.
- **INSERT/UPSERT values, UPDATE SET, key WHERE**: same — placeholders only; `render_insert`
  builds `VALUES (?, ?, …)` and returns `values.to_vec()`; `render_update` emits `col = ?` and the
  WHERE continues the same counter.
- **IN lists / BETWEEN bounds**: one placeholder per element / per bound; bound by index.
- **LIMIT** (the one place a scalar is `format!`-ed into text): `plan.limit` is a typed `i64`,
  clamped `limit.max(0)`, rendered as `{n}`. It is **structural**, never user text — a `String`
  can never reach this arm. Safe.
- **Upsert conflict RHS** (`excluded.{q}` / `VALUES({q})`): `{q}` is `dialect.quote_ident(c)` over a
  **catalog** column, not a value. Safe.
- **Identifiers** (table/schema/column, ORDER BY col): all routed through `Dialect::quote_ident`,
  which doubles the embedded quote char (`"a""b"` / `` `a``b` ``) so an identifier cannot break out
  of its quoting. This is defense-in-depth; the primary guarantee is that identifiers come from the
  **trusted catalog** — `compile::compile` rejects any projected/ordered/filtered name absent from
  `TableCatalog` with `SqlError::UnknownColumn` before the emitter runs, and DML column names come
  from the effect's batch schema (the applier already gated the table via the catalog). The quoting
  test (`identifier_quoting_escapes_embedded_quote`) and the live-sqlite injection test
  (`injection_attempt_is_bound_as_a_parameter_not_executed`, payload `'; DROP TABLE users; --` on
  both write and read paths, table survives) both substantiate the claim.

**Observation 1a (t23 contract obligation, not a t17 defect).** The injection guarantee is split
across a *seam*: `emit` produces `(sql, Vec<Param>)`, but the actual binding lives in the
`SqlBackend` impl. The in-tree sqlite test backend binds correctly (`bind_params` →
`rusqlite::ToSql`). For t23, the HTTP `SqlBackend` MUST send `params` as a structured bound-param
array in the D1 request body — never by string-substituting them into `sql`. The trait doc
("the backend binds `params` positionally — never interpolating a value into `sql`") states this,
but it is a *prose* contract a careless impl could violate. **Proposal:** when t23 lands, add a
backend-level conformance test (analogous to the existing live injection test) that drives the HTTP
backend with the `'; DROP TABLE` payload, so the injection invariant is enforced at *each* backend
boundary, not only at the emitter. Optionally have `render_select`/`render_dml` callers assert
`sql` contains the expected placeholder count to catch a backend that ignores `params`.

## 2. Residual truthfulness (t20 class) — PASS

`compile::lower_predicate` is conservative in the correct direction:

- DROPS residual only for genuinely exact forms: `= <> < > <= >=` (`lower_cmp`), `IN (…)`
  (`lower_in`), `BETWEEN` (`lower_between`), and `AND` of compilable leaves. Each first validates
  the column against the catalog and bails to residual on a bare-col miss / unknown column / dotted
  path (`bare_col`).
- KEEPS as residual: `~` (Match) — explicitly bailed in `lower_cmp` (`op == CmpOp::Match`); empty
  `IN ()` — `set.is_empty()` bail in `lower_in` (correct: `IN ()` is not portable); and the catch-all
  `other =>` arm that covers `LIKE`, `OR`, `NOT`, and any future predicate variant. The catch-all is
  the safe default: an unrecognized predicate is *kept*, never silently dropped. No path drops a
  lossy op → no wrong rows.
- **`AND` semantics are right**: `pushable AND unpushable` pushes the pushable half and keeps the
  other as residual (over-fetch-then-filter); `(None, None)` → no WHERE. The compiled half and the
  residual half are accumulated independently, so the engine re-filter is sound.
- **Emitter dead-arm is honest, not a silent mis-render.** `cmp_op_sql(CmpOp::Match)` returns
  `"= /*unreachable: ~ is kept residual*/"` rather than a plausible-but-wrong `=`. Because the
  compiler never constructs a `Match` `SqlPredicate`, this is dead; if a future refactor *did* leak
  a Match leaf, the embedded comment token makes the resulting SQL fail loudly rather than quietly
  return wrong rows. Acceptable defensive choice. **Observation 2a:** a cleaner long-term shape would
  be to make the unreachability *type-level* — e.g. a `PushableCmpOp` subset enum that structurally
  cannot hold `Match` — so `cmp_op_sql` is total over a type that excludes it and the dead arm
  disappears. Not required for t17; noted for a future tidy.

**Is LIKE genuinely lossy?** Yes — and keeping it residual is correct. qfs `LIKE` is glob
semantics (the compile doc and the `like_predicate_is_kept_as_truthful_residual` test both state
this), whereas SQL `LIKE` is `%`/`_` with dialect-divergent `ESCAPE`/collation/case-fold behavior
(notably MySQL's default case-insensitive collation vs SQLite's `LIKE` case-folding only ASCII vs
Postgres case-sensitive). A single portable `LIKE` rendering with identical semantics across all
three does not exist, so declining is the right call. **Observation 2b (future optimization, t14
territory):** a *glob→LIKE* translation could be pushed exactly per-dialect (escaping qfs glob
metachars and pinning `ESCAPE`/collation), recovering pushdown for a very common predicate. That is
a pushdown-fidelity enhancement, explicitly out of t17 scope (declining is never wrong), but worth a
ticket — LIKE-heavy queries currently full-scan-then-filter.

## 3. Dialect seam for t23 — PASS

The abstraction is exactly the shape t23 needs:

- **`Dialect` is a pure, `Copy`, value-level decision point.** Sqlite already enumerated as a
  first-class variant; its doc even names it "(also the D1 dialect, t23)". Every match over `Dialect`
  is exhaustive (`quote_ident`, `quote_qualified`, `placeholder`, `supports_returning`, `map_type`,
  and the upsert branch in `render_insert`) — **no `_ =>` fallthrough that could silently mis-render**.
  t23 reuses the `Sqlite` arm of every one of these unchanged: `"`-quoting, `?` placeholders,
  `ON CONFLICT (…) DO UPDATE`, native `RETURNING`.
- **`SqlBackend` is the clean substitution point.** It is a narrow trait (`dialect`, `introspect`,
  `execute_read`, `commit_transaction`) returning owned DTOs (`Catalog`, `Row`, `u64`); no vendor
  row type crosses. t23 supplies an HTTP `SqlBackend` that returns `Sqlite` from `dialect()` and
  performs introspect/read/commit over the D1 HTTP API. `ConnHandle`/`ConnRegistry`/`SqlApplier`/
  `compile`/`emit` are all written against the trait + `Dialect`, so they are reused verbatim. The
  test file's `SqliteBackend` (rusqlite, a dev-dep) is itself the proof-of-substitution: it is a
  ~120-line `SqlBackend` impl with no driver-logic changes — t23's HTTP backend is the same exercise
  against a socket.
- **One seam friction for t23 to budget for (Observation 3a):** D1's HTTP API has **no
  client-driven `BEGIN/COMMIT/ROLLBACK`** the way a socket connection does — multi-statement atomicity
  is expressed via D1's batch endpoint, not interactive transaction control. The `SqlBackend`
  contract says `commit_transaction(ops: &[DmlOp])` must apply the batch "inside one ACID
  transaction … ROLLBACK on any error". The signature is *already batch-shaped* (it takes the whole
  `&[DmlOp]` slice, not one op at a time), so the contract maps cleanly onto D1's batch — but t23
  must satisfy the ACID guarantee via the batch endpoint's atomicity, and decline (structured error)
  if a request would need interactive rollback semantics the batch API cannot give. This is a t23
  design note, not a t17 defect: the trait shape is right.
- **`map_type` (Observation 3b):** the SQL-type → `ColumnType` table is shared across dialects with
  a conservative `_ => Unknown` fallback. D1/sqlite report dynamic/affinity types; `Unknown` keeps
  them queryable (late-bound), so no hard failure — fine. No action.

## 4. Secret safety — PASS

- Credential is a `qfs_secrets::Secret` resolved by `(DriverId "sql", AccountId <conn>)` in
  `resolve_dialect`. **Only the scheme prefix is read** (`split("://")` / `split(':')`) for the
  `Dialect`; the remainder (host/user/password/db) is never parsed, retained, or logged — it is
  handed to the backend opaquely.
- The URI/password is **never** in a DTO (`Catalog`/`ColumnDef`/`SelectPlan`/`DmlOp` carry no
  connection field), **never** in `SqlError` (every arm carries a path, a `&'static` verb/op label,
  an identifier name, a scheme *token*, or a fixed reason — `Backend.reason` is documented "never a
  parameter value"), and `Param` is query data, explicitly "not rendered into any log line".
- The `From<SecretError>` reduces to a stable `code` only. The two secret tests
  (`connection_credential_is_never_leaked_in_an_error`, `unknown_scheme_credential_is_rejected_without_leaking`)
  plant a password and assert no `SqlError` Debug/Display surface contains it, while the `Secret`'s
  own Debug/Display redact. `UnknownScheme` carries the scheme token (`"oracle"`) only.

**Observation 4a (discipline, not a current defect).** Secret-freedom of `SqlError::Backend.reason`
and the structured logs is a *convention the backend must uphold*, not a type-enforced one —
`reason: String` could carry anything a backend impl puts in it. The sqlite test backend maps
`rusqlite::Error` into a fixed note correctly. **Proposal for t23 (and the future pg/mysql
backends):** when mapping a vendor error into `SqlError::backend(...)`, map by *error class* (a
fixed `&'static` note per class) rather than forwarding the vendor's `to_string()`, since some
drivers embed the failing statement (and thus bound values) in their error text. A one-line note in
the `SqlBackend` doc ("`reason` must be a class note, never the vendor message verbatim") would pin
the obligation for every future backend author.

## 5. Transaction / DML safety — PASS

- **ACID**: `commit_transaction(&[op])` is the single ACID boundary; the test backend wraps the ops
  in a sqlite transaction and the `multi_effect_commit_is_atomic_and_rolls_back_on_failure` test
  asserts a mid-batch failure leaves zero rows changed. The applier funnels every effect through it.
- **Cross-source**: single-connection = single transaction; `apply_node` resolves exactly one
  `<conn>` per effect and the doc/`SqlError::CrossSource` reserve the structured rejection for a
  multi-conn COMMIT (orchestration deferred to E2). Sound for t17 scope.
- **Keyless UPDATE/REMOVE rejection** (the mass-mutate guard): `build_key_where` builds the WHERE
  only from key columns present in the row; `split_update` and the `Remove` arm both reject a
  `where_.is_none()` with `MalformedEffect` ("would update/delete every row; supply the key
  column(s)"). This is the right structural defense — an accidental whole-table mutation cannot be
  lowered. `UPDATE` additionally rejects a row with no non-key column to SET.
- **View-write rejection at BOTH layers**: parse-time capability gate (`caps_for` →
  `Capabilities::from_verbs(&[Select])` for a view, `check_capability` test asserts
  `unsupported_verb` for writes) AND the applier belt-and-suspenders (`is_view()` →
  `ReadOnlyView`, `applier_rejects_write_to_a_view_belt_and_suspenders` test). A hand-built plan
  bypassing the gate is still rejected at apply. Good.
- **Upsert conflict target from key columns**: `EffectKind::Upsert` derives `conflict_keys` from
  `table.key_columns()` (PK, else unique) and rejects a keyless table (`MalformedEffect`,
  "requires a primary-key or unique column to be retry-safe"). `render_insert` then emits the
  dialect-correct conflict clause (`ON CONFLICT (keys) DO UPDATE` for pg/sqlite, `ON DUPLICATE KEY
  UPDATE` for mysql), updating exactly the non-key columns; the empty-update edge (all columns are
  keys) degrades to `DO NOTHING` / a no-op `key = key` so the statement stays a valid idempotent
  upsert. `upsert_is_retry_safe_running_twice_yields_one_row` confirms idempotency.

**Observation 5a (mysql upsert correctness gap to flag for the future mysql backend, not a t17
runtime defect).** `key_columns()` returns the PK *or* all unique columns, and `render_insert`'s
mysql arm `ON DUPLICATE KEY UPDATE` fires on **any** unique-key collision, ignoring *which* key the
caller named in `conflict_keys` — whereas pg/sqlite `ON CONFLICT (key_list)` targets the *named*
key. For a table with multiple unique constraints these diverge: pg/sqlite upsert on the named key
only, mysql upserts on whichever unique key collides. Since the live ACID/upsert tests run on
sqlite, this dialect divergence is unexercised. It is faithful to mysql's actual capability (mysql
genuinely cannot scope `ON DUPLICATE KEY` to one named constraint), so it is not a *bug* — but it is
a *semantic divergence the planner should know about*. **Proposal:** document this in the
`render_insert` mysql arm (one comment) and, when the live mysql backend lands, add a golden/behavior
test for the multi-unique-key case so the divergence is an explicit, reviewed contract rather than a
latent surprise.

## 6. Spine — PASS

`dep_direction.rs` confirms `qfs-driver-sql` is admitted as a **runtime-leaf consumer** in both the
generic leaf-confinement check (b) and the named allowlist (b'): it depends on `qfs-runtime` (to
bridge its sync `SqlApplier` to the async `ApplyDriver` via `PlanApplierBridge`), and nothing
depends back onto it, so tokio dead-ends in the leaf and cannot transit into the pure spine. The
driver composes through the `Driver` contract (`mount() = /sql`, archetype + typed `Schema` from the
catalog, per-node `Capabilities`, `PushdownProfile::Partial` all-true) — a clean allowlist-composed
runtime leaf. `Cargo.toml` keeps rusqlite (bundled) as a **dev-dependency** test backend, so no
vendor SQL client enters the production dependency closure of the driver; the live backend is
injected via the `SqlBackend` trait. Consistent with RFD §9 "no heavy vendor SDKs in the spine".

---

## Summary of observations (all forward-looking; none blocks t17)

| # | Theme | Proposal | Owner |
|---|-------|----------|-------|
| 1a | Injection invariant lives partly in the backend impl | Add a per-backend injection conformance test (drive HTTP backend with `'; DROP TABLE`) | t23 |
| 2a | Match dead-arm in `cmp_op_sql` | Type-level subset enum (`PushableCmpOp`) so the arm disappears | future tidy |
| 2b | LIKE always residual | Ticket a per-dialect glob→LIKE exact pushdown (escape + pin ESCAPE/collation) | t14 follow-up |
| 3a | D1 has no interactive BEGIN/COMMIT | Map `commit_transaction(&[op])` onto D1 batch atomicity; decline what the batch API can't give | t23 |
| 4a | `SqlError::Backend.reason` secret-freedom is convention | Map vendor errors by class to a `&'static` note; doc the obligation on `SqlBackend` | t23 / pg-mysql |
| 5a | mysql `ON DUPLICATE KEY` ignores named conflict target | Comment the divergence; add a multi-unique-key golden when the mysql backend lands | future mysql backend |

**Verdicts:** Injection safety — PASS (every value bound; identifiers from trusted catalog +
doubled-quote escaping; one backend-side obligation for t23). Residual truthfulness — PASS (no lossy
op dropped; LIKE genuinely lossy and correctly kept). t23 reuse — PASS (sqlite dialect emitter +
HTTP `SqlBackend` is a clean, exhaustive substitution; budget for the D1 batch-vs-interactive-txn
seam).
