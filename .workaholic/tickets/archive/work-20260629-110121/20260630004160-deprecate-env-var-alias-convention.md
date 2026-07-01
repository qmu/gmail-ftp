---
created_at: 2026-06-30T00:42:00+09:00
author: a@qmu.jp
type: refactoring
layer: [Config, UX]
effort: 4h
commit_hash: 8ecdc10
category: Changed
depends_on: [20260630004140-connection-registry-from-declarations.md]
---

# Deprecate the QFS_SQL_* / QFS_GIT_* alias convention

Part of EPIC `20260630004100`. Retire implicit env-var alias loading in favor of declarations.

## Implementation steps

1. With the registry now declaration-driven (`…004140`), the `QFS_SQL_<conn>` / `QFS_GIT_<repo>`
   scan becomes a **compatibility shim**: still recognized, but emits ONE `info`-level deprecation
   notice naming the equivalent `CREATE CONNECTION` (NOT a per-statement WARN — see the t8 noise
   lesson). Gate removal behind a follow-up.
2. **`qfs connection import-env`** helper: read the current `QFS_SQL_*` / `QFS_GIT_*` env and print
   the equivalent `CREATE CONNECTION …` block to stdout, so a user migrates by piping it into a
   `connections.qfs`.
3. Update `--help`/`installation.md` to point at the declaration model; mark the env vars deprecated.
4. Tests: an env-only connection still resolves (shim) and emits exactly one deprecation notice; the
   `import-env` output round-trips back into a working connection.

## Key files

- `crates/qfs/src/{sql.rs,git.rs}` (the shim), `crates/cmd/src/lib.rs` (`import-env`).

## Considerations

- Keep the shim for one release; schedule hard removal in a follow-up so existing setups don't break
  on upgrade. SemVer: removing the env convention is a surface change — announce it.
