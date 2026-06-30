---
created_at: 2026-06-30T20:31:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort: 4h
commit_hash: 176ffbf
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

## Final Report

Development completed AND verified LIVE against real Postgres 16 + MariaDB 11 (podman). `/sql` is no
longer SQLite-only: a declared `CREATE CONNECTION pg DRIVER postgres AT '<url>'` (or `mysql`) now
connects, introspects, and runs pushed `WHERE`/projection/`ORDER BY`/`LIMIT`. Live-proven:
`/sql/pg/widgets |> where qty > 15 |> select name, qty |> order by qty desc` → `gamma 30, beta 20`;
full-row type mapping `{id, name, qty, price, active}` → int/text/int/float/bool; identical on MySQL.

Delivered: `crates/qfs/src/sql_backends.rs` (the `PostgresBackend` + `MysqlBackend` `SqlBackend`
impls), `conn_registry` wiring (`crate::sql`) over declared `postgres`/`mysql` connections with
`env:`-scheme password resolution, `deploy/dev/compose.yml` + seeds + an example `connections.qfs`,
and hermetic mapping tests. Pure-Rust clients confined to the terminal binary; the dep-direction
guard stays green.

### Discovered Insights

- **Insight (load-bearing)**: the sync `postgres` crate wraps `tokio-postgres` and drives its OWN
  tokio runtime, which PANICS ("cannot start a runtime from within a runtime") when called from
  inside qfs's async read executor. The fix is to run the Postgres client on a DEDICATED OS thread
  (no outer runtime) and talk to it over channels — an actor. The `mysql` crate is pure-sync, so it
  needs none of this. Any future tokio-wrapping sync engine client needs the same isolation.
  **Context**: `PostgresBackend` is a channel actor; `MysqlBackend` is a plain `Mutex<Conn>`.
- **Insight**: rust-postgres is STRICT about bind types — a bare `i64` is rejected against an `int4`
  column even though Postgres itself compares `int4 > bigint` fine. A `ToSql` adapter (`PgBind`) that
  encodes an integer/float param as whatever type the server INFERS for the placeholder fixes it
  without per-query type knowledge. (MySQL's text protocol sidesteps this.)
  **Context**: This is why a `where qty > 15` failed until the adapter landed; a bare `i64` bind is a
  trap for any int4/int2 column.
- **Insight**: `--`-style comments in a `connections.qfs` break the multi-statement parse (the leading
  comment block swallows the first statement, leaving only the last). The shipped
  `deploy/dev/connections.qfs` is therefore kept **comment-free** (it parses + loads both connections,
  verified) with all guidance moved to `deploy/dev/README.md`. **Follow-up: support `--` line comments
  in the `connections.qfs` parser** (`crates/core/src/ddl/connections.rs`) — a config format should
  allow comments.
