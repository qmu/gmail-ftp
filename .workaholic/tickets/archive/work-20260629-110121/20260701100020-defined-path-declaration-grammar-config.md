---
created_at: 2026-07-01T10:00:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash: c9c0604
category: Added
depends_on: [20260701100010-design-defined-path-model-grammar.md]
---

# `CONNECT` / `DISCONNECT` grammar + the persisted binding

Part of EPIC `20260701100000`. Implements the statement decided in the keystone (`100010`). This
ticket adds the SYNTAX and the SIDE EFFECT (writing the binding to the DB); resolution is `100030`,
registration is `100040`.

## Semantics (from `100010`)

`CONNECT` is a **side-effecting statement** — running it mutates the qfs server state (the project
DB). There is **no `connections.qfs` file**. Two arms, disambiguated by what follows `TO`:

- **Full connect:** `CONNECT /<path> TO <driver> [AT '<locator>'] [SECRET '<ref>']`
- **Alias only:** `CONNECT /<path> TO /<existing-path>` (no driver, no secret — reuse the connection)
- **Remove:** `DISCONNECT /<path>`

## Plan

1. **Parser AST + grammar.** Add `CONNECT`/`DISCONNECT` as **contextual idents** (`word(...)`, like
   `SECRET`/`AT` — no new frozen keyword) in `crates/parser/src/{ast.rs,grammar.rs}`. The path is a
   typed value object (multi-segment allowed), not a bare string (`type-driven-design`). The `TO`
   target parses as either a driver ident (full) or a `/`-path (alias); pick the `TO` token per the
   keystone's open sub-decision (`TO` proposed). Parse tests in `crates/parser/src/tests.rs`.
2. **The effect.** `CONNECT`/`DISCONNECT` lower to a commit-class effect (they change server state, so
   they ride the describe→preview→commit path, not a read). Model the effect + its apply leg so
   `qfs run --commit "CONNECT …"` persists it.
3. **Persist the binding (server state = DB, no file).** Add a binding table to the project DB (a new
   migration in `crates/store` / the project schema): `{path, canonical driver-id, connection, AT
   locator}` — NON-secret metadata; an alias is another row pointing at the same connection. The
   secret VALUE follows the reference: `SECRET 'env:VAR'` stores only the ref (value read from env at
   use, never persisted); `SECRET 'vault:…'` is sealed envelope-encrypted in the existing
   `secret_store`. Wire this through `crates/qfs/src/connection.rs` + `secret_ref`.
4. **CLI.** `qfs connect …` / `qfs disconnect …` (and `qfs connection list` reads the binding table).
5. **Docs.** Regenerate the language/driver docs; `gen-docs --check` green.

## Key files

- `crates/parser/src/{ast.rs,grammar.rs,tests.rs}`, `crates/lang`, the effect/plan wiring, the binding
  migration in `crates/store` + the project schema, `crates/qfs/src/{connection.rs,secret_ref.rs}`.

## Considerations

- A parse-coverage test proves `CONNECT`/`DISCONNECT` parse AND the frozen keyword set is unchanged.
- The binding row is selectors/metadata only (no secret), like the existing connection rows; the
  secret stays in `secret_store` (envelope-encrypted) or is an env reference.
- Do NOT wire resolution/registration here — this ticket only DECLARES + STORES the binding.

## Policies

- `implementation/type-driven-design` (typed path value object, contextual-ident grammar),
  `implementation/persistence` (binding schema + FK to the secret/connection + `DISCONNECT` deletion
  semantics), `design/data-sovereignty` + `design/defense-in-depth` (the credential half),
  `implementation/directory-structure`/`coding-standards`.
