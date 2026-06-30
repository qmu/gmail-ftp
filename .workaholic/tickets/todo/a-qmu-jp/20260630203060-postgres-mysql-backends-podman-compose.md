---
created_at: 2026-06-30T20:31:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# Postgres / MySQL `/sql` backends + a podman compose dev stack (owner item #1)

## What's wanted

A `podman compose` file that runs **MySQL + PostgreSQL** for dev, and a **dev connection** so qfs
actually connects to them. Today the binary's `/sql` is **SQLite-only** (`crate::sql::SqliteBackend`
via `rusqlite`); a declared `CREATE CONNECTION analytics DRIVER postgres AT 'postgres://...'` parses
but cannot connect.

## Plan

1. `deploy/dev/compose.yml` (podman; `podman 5.8.4` + `podman-compose` are installed): Postgres +
   MySQL services with seeded dev databases + a `connections.qfs` example.
2. Implement `SqlBackend` for Postgres and MySQL in the binary (`crate::sql`) — `qfs-driver-sql` is
   the vendor-free trait+compiler; the production backends live in the binary (the
   `SqliteBackend` precedent, kept off the dep guard's lower spine). Choose the driver crate
   (`tokio-postgres`/`postgres`, `mysql`/`mysql_async`, or `sqlx`); confirm the dialect already
   exists in `qfs-sql-core` (`Dialect::{Postgres,Mysql}` — `render_select`/`render_dml`).
3. `crate::sql::conn_registry()` builds a Postgres/MySQL handle from a declared
   `DRIVER postgres|mysql AT '<url>' SECRET '<ref>'` (the password via `crate::secret_ref`).

## Key files

- `crates/qfs/src/sql.rs` (backends + `conn_registry`), `crates/sql-core/src/{dialect,emit,compile}.rs`,
  `crates/driver-sql/src/conn.rs` (`SqlBackend` trait). New `deploy/dev/compose.yml`.

## Considerations

- Secret resolution is in place (`crate::secret_ref::resolve_secret_ref`, commit `da3f187`) — use it
  for the DB password from `SECRET 'env:PG_PASSWORD'` / `vault:...`.
- Keep the new DB-driver crate confined to the binary (terminal leaf) so the dep-direction guard
  (`crates/cmd/tests/dep_direction.rs`) stays green; add it to the allowlist deliberately.
- Live-testable here (podman available). Add a hermetic golden-SQL test per dialect (no live server)
  + an opt-in live test gated on the compose stack.
