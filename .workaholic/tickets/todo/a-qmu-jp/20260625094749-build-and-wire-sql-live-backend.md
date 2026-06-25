---
created_at: 2026-06-25T09:47:49+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: []
---

# Build + wire a live sql backend (real DB connection)

## STATUS (2026-06-25): SQLite backend DONE (v0.0.5)

The SQLite slice is shipped: a real, file-backed `SqlBackend` (rusqlite, confined to the terminal
binary in `crates/qfs/src/sql.rs`), wired into the commit registry + planning mount, configured via
`QFS_SQL_<conn>=<path>`. Verified end-to-end against a real temp DB (independent `sqlite3` CLI):
INSERT / UPDATE / REMOVE all apply; bound-param injection safety; ACID; unconfigured conn fails
closed. This also required fixing the engine-wide dropped-VALUES-payload bug (see the commit). The
Postgres/MySQL networked backends (needing a live DB to verify) remain as the residual of this
ticket. Rusqlite-bundled was already in the offline cache.

## Overview

Unlike github/slack/Google/objstore (whose production clients exist and only need wiring), the sql
driver has **only the `SqlBackend` trait + a mock** — no production database backend was built.
`crates/driver-sql/src/conn.rs` defines `SqlBackend` (trait), `ConnHandle`, `ConnRegistry`; the
applier/compiler/dialect/catalog are all real, but nothing connects to a real database.

So this is **build a production `SqlBackend` impl**, then wire it — not just wiring.

## Exact seams

- **Build:** a production `impl SqlBackend` over a real client. Decide the client + dialect scope
  (Postgres/SQLite/MySQL) and the dependency (e.g. `rusqlite` for SQLite is offline-friendly and the
  smallest footprint per ADR-0002/0003; a networked Postgres client is heavier). Confine it to its
  own leaf crate (e.g. `qfs-driver-sql-live`) so any heavy/async dep dead-ends there, mirroring how
  reqwest is confined to `qfs-driver-http`.
- **Config:** connection strings / DSNs per `/sql/<conn>/...` name. Source from the credential store
  + config (decide env names). `ConnRegistry::with(<conn>, ConnHandle::new(Arc<dyn SqlBackend>))`.
- **Wire:** `SqlDriver::new(registry)` → `sql_apply_driver` → `commit.rs` `live_registry()` under
  DriverId `sql`; cred-free planning mount in `run_engine_and_reads`. Catalog resolution needs a
  registered `TableCatalog` (a registration requirement — see `SqlDriver::resolve_table`).
- **Dep direction:** the new live crate is a binary-only leaf (allowlist in
  `crates/cmd/tests/dep_direction.rs`); `qfs-cmd`/`qfs-exec` stay off it.

## Verification

- Unit/integration: against an in-process SQLite (real, offline) if SQLite is chosen — genuinely
  E2E-verifiable here, unlike the networked drivers.
- A networked DB (Postgres) needs a live DB to verify.

## Considerations

- ADR-0002/0003 dependency-footprint + offline rules: prefer the smallest real backend; SQLite is
  the most verifiable-offline choice and a good first slice.
- DSNs/passwords are `Secret`; never logged/argv.
- Patch bump + docs-in-lockstep per the umbrella ticket.
