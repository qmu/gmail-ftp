---
created_at: 2026-06-30T00:42:10+09:00
author: a@qmu.jp
type: refactoring
layer: [UX, Infrastructure]
effort: 4h
commit_hash: 1368c0c
category: Changed
depends_on: [20260630004130-secret-reference-resolution.md, 20260630004140-connection-registry-from-declarations.md]
---

# Align `qfs connection` with the declaration model

Part of EPIC `20260630004100`. The CLI stores SECRET VALUES; declarations own the alias.

## Implementation steps

1. **`qfs connection add <driver> <name>`** keeps storing a secret value in the vault — but it now
   feeds a `SECRET vault:<…>` reference in a `CREATE CONNECTION`, not an implicit binding. Document
   that `add` provisions the *secret*, and a `CREATE CONNECTION … SECRET vault:<…>` declares the
   *alias* that uses it.
2. **`qfs connection list`** shows the declared connections (name, driver, locator, secret-ref —
   never the secret) by reading the loaded `ConnectionSet`, not just raw store rows.
3. **`connection use`** — reconcile with the design ADR's default-connection decision (if the name
   is always in the path, `use` sets the *default* a bare `/mail` resolves to; otherwise deprecate).
4. `rotate`/`revoke`/`rekey` stay (they manage the vault secret a `vault:` ref points at).
5. Tests: `add` then a `SECRET vault:…` connection binds; `list` shows the declared set; `revoke`
   makes a declared connection fail closed.

## Key files

- `crates/qfs/src/connection.rs`, `crates/cmd/src/lib.rs`, `/sys/connections` admin view.

## Considerations

- The mental model after this epic: **declare the alias in the language; store the secret in the
  vault; reference it by `vault:`** — one coherent path, no naming-convention magic.
