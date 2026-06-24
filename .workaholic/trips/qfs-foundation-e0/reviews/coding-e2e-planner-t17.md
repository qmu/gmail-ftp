# Coding E2E (Planner) — t17 SQL databases driver

Author: Planner (Progressive)
Role: E2E / external testing only (no code review)
Target: t17 — `qfs-driver-sql` (`SqlDriver`, `SqlBackend`, `Dialect`, `ConnRegistry`, `SqlApplier`,
`compile`, `render_select`/`render_dml`, `sql_apply_driver`)
Method: a throwaway external consumer crate (`/tmp/t17-e2e`, own `[workspace]`, path-deps on
driver-sql/runtime/driver/plan/types/secrets + rusqlite) implementing its **own** `SqlBackend`
over `rusqlite` (production never depends on rusqlite) against a live in-memory SQLite seeded with
a `products` table + a `cheap` view. Driven entirely through the public API + the runtime bridge.
The backend also captured every executed SQL string so pushdown and bind-only could be proven from
the outside. Removed after the run.

Seed: `products(id PK, name TEXT NOT NULL, price INTEGER, in_stock BOOLEAN)` with 4 rows
(apple/100, banana/50, cherry/300, date/200); `CREATE VIEW cheap AS SELECT id,name FROM products
WHERE price < 150`.

## Verdict: E2E approved

15/15 external checks PASS, 0 failures. No panics on adversarial input. No secret leak. The
injection attempt did NOT drop the table and was stored/matched verbatim as a bound parameter.

## Result per item

### 1. Introspect + SELECT pushdown — PASS
- `DESCRIBE /sql/shop/products` returns the typed catalog schema: `id:Int`, `name:Text` (NOT NULL),
  `price:Int` (nullable), `in_stock:Bool`; archetype `RelationalTable`; PK `id` introspected
  (`key_columns().len() == 1`).
- `SELECT name WHERE price > 80 ORDER BY price DESC LIMIT 2` → `["cherry","date"]`, residual `None`.
- Compiled SQL (captured from the backend, value bound not interpolated):
  `SELECT "name" FROM "products" WHERE "price" > ? ORDER BY "price" DESC LIMIT 2`
  — WHERE + ORDER BY + LIMIT all pushed down; the literal `80` is a bound `?`, absent from the text.

### 2. Residual semantics — PASS
- `LIKE 'a%'` is kept as a local residual (`residual.is_some()`); the driver over-fetched ALL 4 rows
  with NO `WHERE` pushed (captured SQL: `SELECT "name" FROM "products"`), so the engine re-filters
  exactly — lossy op over-fetches, never returns wrong rows.
- Exact `=` / `IN` / `BETWEEN` push fully, residual `None`: `name = 'apple'` → 1 row;
  `id IN (1,3)` → 2 rows; `price BETWEEN 60 AND 250` → 2 rows.
- Mixed `price > 60 AND name ~ 'a'`: the `>` leaf compiles into the SQL `WHERE`; the `~` regex
  (non-portable) is kept as the residual — exact half pushed, lossy half retained.

### 3. Injection safety (BLOCKING) — PASS
- Payload `'); DROP TABLE products; --` was INSERTed as a `name`, and the same string was used as a
  WHERE-filter value on a SELECT.
- Table is **NOT dropped** (`sqlite_master` still lists `products`; row count went 4→5 from the
  INSERT, never to 0).
- The literal is stored verbatim (`SELECT name WHERE id=99` returns the exact evil string) and
  matched verbatim (`SELECT id WHERE name = <evil>` returns `id=99`, one row).
- Captured SQL proves bind-only — no value text in either statement:
  - INSERT: `INSERT INTO "products" ("id", "name", "price", "in_stock") VALUES (?, ?, ?, ?)`
  - READ: `SELECT "id" FROM "products" WHERE "name" = ?`
  - Neither string contains `DROP`; both carry only `?` placeholders.

### 4. Effects — PASS
- INSERT (id 5) → affected 1, count 4→5; UPDATE by key id=5 → name becomes `eggplant`;
  DELETE (REMOVE) by key id=5 → count back to 4. Right rows, right columns.
- Multi-effect atomic commit: two INSERTs commit atomically (count 4→6).
- Forced-failure rollback: a 2-op commit whose 2nd INSERT violates the PK (dup id 10) fails with a
  `backend` error and the FIRST INSERT is rolled back — count stays 6 (no partial write).
- Keyless mass-mutate rejection: a keyless UPDATE and a keyless DELETE are both **rejected** (bridged
  to non-retryable `EffectError::Terminal`); reasons: "UPDATE without a key filter would update
  every row; supply the key column(s)…" and "REMOVE without a key filter would delete every row…".
  Row count unchanged (6) — no mass mutation occurred.

### 5. Capability (view rejects writes structurally) — PASS
- Table `products` admits full CRUD (`SELECT/INSERT/UPSERT/UPDATE/REMOVE` all pass the gate).
- View `cheap` admits `SELECT`; every write verb is rejected at the parse-time gate with code
  `unsupported_verb`.
- Belt-and-suspenders: a hand-built INSERT effect targeting the view (bypassing the parse gate) is
  rejected at the applier with code `capability_denied` — no I/O on the view.

### 6. Secret safety — PASS
- Planted credential `postgres://user:PLANTED-PASSWORD-9f8e7d6c@db.internal:5432/app` stored as a
  `Secret`. `resolve_dialect` reads only the scheme → `Dialect::Postgres`.
- Across all surfaces — `SqlError` Debug + Display (UnknownConnection, backend), `Secret` Debug +
  Display, `Dialect` Debug — none contains `PLANTED-PASSWORD`, the `9f8e7d6c` fragment, or the
  `user:` userinfo. The `Secret` rendered its redaction marker instead of the value.

### 7. End-to-end COMMIT + no panic — PASS
- `sql_apply_driver(&driver)` → `PlanApplierBridge`; `bridge.apply_one(EffectInput::from_node(&ins),
  &ApplyCx::default())` committed an INSERT (id 42, `fig`): `affected == 1`, DB shows the row
  (`SELECT name WHERE id=42` → `fig`), count 4→5 — the async interpreter→bridge→applier→ACID-txn
  path executed end-to-end.
- Adversarial SELECT of an unknown column returns a structured `unknown_column` error (not a panic).

## Observation + proposal (Critical Review Policy)

- Observation: a keyless UPDATE/DELETE is correctly rejected, but through the `SharedApplier` bridge
  the structured `SqlError::MalformedEffect` discriminant is flattened to the generic
  `EffectError::Terminal` (code `terminal`); the specific cause survives only in the reason text.
  The security property (no mass-mutate) is fully intact, so this does not block.
- Proposal: for AI-consumable recovery (RFD §5), consider mapping the keyless-write guard to a more
  specific terminal discriminant (e.g. a `MalformedEffect`/`PreconditionRequired` code) so an agent
  can branch on "supply a key" without parsing reason text. A non-blocking enhancement for a later
  iteration.

## Evidence summary

- Injection-safe: `products` table intact, 5 rows after the evil INSERT, value stored/matched
  verbatim; compiled SQL is placeholder-only (`?`), no `DROP` text, no value interpolation.
- Rollback: failed multi-effect commit left the count at 6 (first INSERT undone).
- Secret-absent: no `PLANTED-PASSWORD` / `9f8e7d6c` / `user:` in any Debug/Display/error surface;
  redaction marker present.

A successful injection or a secret leak would BLOCK; neither occurred. **E2E approved.**
