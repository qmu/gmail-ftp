---
created_at: 2026-06-30T00:41:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Config, UX]
effort:
commit_hash: 0f7bf06
category: Changed
depends_on: [20260630004140-connection-registry-from-declarations.md]
---

# Load CREATE CONNECTION declarations from config (serve / job / one-shot)

Part of EPIC `20260630004100`. Make declarations discoverable and loaded.

## Sub-tasks (each a ≤4h commit)

1. **`.qfs` config** (`crates/core/src/ddl/server/spec.rs`): accept `CREATE CONNECTION` alongside
   `CREATE TRIGGER`/`CREATE POLICY`, collected into a `ConnectionSet` the boot path hands to the
   registry builder (`…004140`). `qfs serve <cfg>` / `qfs job <cfg> <name>` load them.
2. **One-shot loading**: a default connections config path (e.g.
   `$XDG_CONFIG_HOME/qfs/connections.qfs`) auto-loaded by `qfs run`, plus an explicit
   `qfs run --config <file>` flag. Absent config = no connections (fails closed, honest error).
3. **`qfs describe`/run stay cred-free**: loading a config resolves no secrets; only a read/commit
   pulls them.
4. **Tests**: a config with two SQLite + one git connection makes all three paths readable; a cloud
   connection with `SECRET vault:…` registers; a malformed decl reports a clear config-parse error
   with a line number.

## Key files

- `crates/core/src/ddl/server/spec.rs`, `crates/cmd/src/lib.rs` (the `--config` flag + default path),
  `crates/qfs/src/{serve,shell}.rs`.

## Considerations

- This is the "explicit, reviewable, versionable" payoff: a `connections.qfs` you commit to a repo.
- Keep the parse-error message line-located and secret-free.
