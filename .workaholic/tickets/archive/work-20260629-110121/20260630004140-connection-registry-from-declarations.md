---
created_at: 2026-06-30T00:41:40+09:00
author: a@qmu.jp
type: refactoring
layer: [Infrastructure]
effort:
commit_hash: ecd73ed
category: Changed
depends_on: [20260630004120-parser-create-connection-statement.md, 20260630004130-secret-reference-resolution.md]
---

# Build the mount + read/apply registries from declared connections

Part of EPIC `20260630004100`. Replace the env-var naming-convention scan with declaration-driven
registry construction. Multi-day — see sub-tasks.

## Overview

Today `crate::sql::conn_registry` / `crate::git::git_driver` scan `QFS_SQL_*` / `QFS_GIT_*` and the
Google/objstore wiring keys off `active_connection` + the credential store. Make a set of
`ConnectionDecl`s (from the config, `…004150`) the single source: each decl → a mount + read facet +
apply facet for `/sql/<name>`, `/git/<name>`, `/mail/<name>`, etc., with its `SECRET` resolved
lazily via `…004130`.

## Sub-tasks (each a ≤4h commit)

1. **A `ConnectionSet` → registries builder** in the binary: map each decl to the right driver
   constructor (`SqliteBackend`/`Repo`/`GoogleApi*Client`/objstore) using `AT` for the locator and
   `resolve_secret` for credentials; mount under the driver-family path with `<name>` as the segment.
2. **Re-point** `crate::{sql,git,google,objstore}` to consume the `ConnectionSet` instead of env
   scanning; keep the read-facet registration (t3–t7 facets) keyed by the declared name.
3. **Default connection** handling (a bare `/mail` → the declared default) per the design ADR.
4. **Tests**: a declared SQLite connection reads end-to-end; a declared git repo reads; a declared
   cloud connection with a `vault:` secret binds (mock client); an undeclared `/sql/x` fails closed.

## Key files

- `crates/qfs/src/{sql.rs,git.rs,google.rs,objstore.rs,shell.rs,commit.rs}`.

## Considerations

- Behavior-preserving for a connected user; the *source* of the connection moves from env to decl.
- Keep `crate::sql::seeded_test_driver` / `git` fixtures working (or port them to declarations).
