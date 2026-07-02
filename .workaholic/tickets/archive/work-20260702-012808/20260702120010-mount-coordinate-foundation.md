---
created_at: 2026-07-02T12:00:10+09:00
author: a@qmu.jp
type: enhancement
layer: [DB, Domain]
effort:
commit_hash: 9555d9e
category: Added
depends_on: []
---

# Mount-coordinate foundation: `host` + `account` on the path binding (schema v9 + grammar)

Part of EPIC `20260702120000` (ADR 0008). The mount becomes the carrier of the full
**(host, driver, account)** coordinate — the storage and grammar groundwork every later ticket
builds on. No behavior changes yet: the columns exist, parse, and round-trip; nothing reads them
for bind resolution until `20260702120050`.

## Steps

1. **Migration v9** (`packages/qfs/crates/store/src/lib.rs` PROJECT_MIGRATIONS, currently v8): a new
   schema file (e.g. `schema/project_path_bindings_v9.sql`) — `ALTER TABLE path_binding ADD COLUMN
   host TEXT NOT NULL DEFAULT 'local'` and `ADD COLUMN account TEXT` (nullable — local sources have
   none). Never edit the shipped v8 body (checksum guard, `migrate.rs`). Extend the table-existence
   tests (`lib.rs:715-750`).
2. **Row + I/O** (`packages/qfs/crates/qfs/src/path_binding.rs`): `PathBindingRow` (line 25) gains
   `host: String` + `account: Option<String>`; thread through `db_upsert_binding` (46),
   `db_list_bindings`, `db_get_binding`; extend the upsert/alias/FK tests (157-217).
3. **Grammar** (`packages/qfs/crates/parser/src/grammar.rs`): add `ACCOUNT '<value>'` and
   `HOST '<value>'` clauses to `connect_stmt` (1105) / `connect_secret_clauses` (1141), extend
   `PATH_BINDING_COLUMNS` (1029) + `binding_values` (1055). **Contextual idents like `AT`/`SECRET`
   (comment at 1016) — not frozen keywords.** Parser tests for `CONNECT /mail DRIVER gmail ACCOUNT
   'you@gmail.com'` and the `HOST` form.
4. **Desugar threading** (`packages/qfs/crates/qfs/src/sys.rs` 319-343 + 147-150): carry
   account/host through the `/sys/paths` UPSERT and the listing columns.
5. **CLI surface** (`packages/qfs/crates/qfs/src/connection.rs` `run_connect` @537 and
   `packages/qfs/crates/cmd/src/lib.rs` Connect @338): accept an account argument and `--host`
   (default `local`), store them on the binding.

## Key files

- `packages/qfs/crates/store/src/lib.rs` (PROJECT_MIGRATIONS v8→v9), `store/src/migrate.rs`
  (append-only invariant), `store/src/schema/project_path_bindings.sql` (FROZEN — reference only)
- `packages/qfs/crates/qfs/src/path_binding.rs`, `src/sys.rs`, `src/connection.rs` (`run_connect`)
- `packages/qfs/crates/parser/src/grammar.rs`, `packages/qfs/crates/cmd/src/lib.rs`

## Considerations

- The `qfs host` verb itself is `20260702120060`; here only the **column** and the `--host` flag
  exist, always `'local'` in practice.
- Keep the schema-first order: write v9, then the row, then the grammar (persistence policy).
- The cookbook ratchet (`crates/test/tests/cookbook_skills.rs`) will parse any new recipe text —
  do not add `ACCOUNT` recipes to cookbooks in this ticket (docs land in `20260702120070`).

## Quality Gate

Global gate (EPIC) plus:

- New v9 table-existence + column assertions pass; migration ledger stays append-only (checksum
  test green).
- Parser round-trip tests: `CONNECT … ACCOUNT '…'` / `… HOST '…'` parse; `ACCOUNT`/`HOST` still
  usable as ordinary identifiers elsewhere (contextual-ident regression test).
- `path_binding` upsert/list/get round-trips host+account; FK behavior unchanged.
