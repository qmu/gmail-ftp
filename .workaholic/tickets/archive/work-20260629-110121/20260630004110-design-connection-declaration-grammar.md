---
created_at: 2026-06-30T00:41:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: 4h
commit_hash: f43c675
category: Changed
depends_on: [20260630004100-epic-in-language-connection-declaration.md]
---

# Design: the CREATE CONNECTION grammar + semantics (keystone)

Part of EPIC `20260630004100`. Resolve the design questions before any code; everything depends on it.
The deliverable is a written spec (an ADR under `docs/adr/` + the grammar sketch), not code.

## Decisions to make

1. **Statement shape.** Confirm `CREATE CONNECTION <name> DRIVER <driver> [AT '<locator>'] [SECRET
   <ref>]` (vs. a `CONNECT <path> TO …` form). Keep it consistent with `CREATE TRIGGER`/`CREATE
   POLICY` (parser `ast.rs`, `crates/core/src/ddl/server/spec.rs`).
2. **Driver → path family map.** `sqlite|postgres|mysql → /sql`, `git → /git`, `gmail → /mail`,
   `gdrive → /drive`, `github → /github`, `slack → /slack`, `s3 → /s3`, `r2 → /r2`. The connection
   `<name>` is the `<conn>` path segment.
3. **`<conn>`-in-path vs active-connection.** Today `/sql/<conn>` puts the connection in the path but
   `/mail` uses an `active connection` selector (`qfs connection use`). Pick ONE coherent model —
   recommendation: the name is always in the path (`/mail/work/…`), with an optional default so a
   bare `/mail` resolves to a declared default connection. Reconcile `connection use`.
4. **Secret reference scheme.** `SECRET env:<VAR>` and `SECRET vault:<path>`. No inline literal is
   accepted (parse error). Resolution timing (at registry build vs. at use), and the secret-free
   failure when a ref is missing/locked.
5. **Where declarations live.** The `.qfs` config (loaded by `serve`/`job`, and `qfs run --config`);
   a default config path (e.g. `$XDG_CONFIG_HOME/qfs/connections.qfs`). Decide whether a connection
   can also be declared at runtime via `qfs run` (it mutates the mount table — likely config-only at
   first).
6. **describe/preview semantics.** `describe /sql/orders/t` must stay cred-free; declaring a
   connection performs no I/O; the SECRET is resolved only when a read/commit actually runs.
7. **Migration.** How `QFS_SQL_*` / `QFS_GIT_*` map to declarations, the deprecation path, and the
   `qfs connection import-env` helper output.

## Key files (reference)

- `crates/parser/src/ast.rs` (Statement, the CREATE family), `crates/lang/src/keywords.rs`.
- `crates/core/src/ddl/server/spec.rs` (`.qfs` config statements), `crates/qfs/src/{sql,git,google,
  objstore,connection}.rs` (today's env-var + credential-store wiring).

## Considerations

- The grammar is part of the **versioned surface** — design additively and deliberately.
- Output: a short ADR + the finalized grammar EBNF snippet the parser ticket implements.
