---
created_at: 2026-06-30T00:42:20+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 4h
commit_hash: 6eac7b9
category: Changed
depends_on: [20260630004150-config-and-runtime-loading.md, 20260630004170-connection-cli-alignment.md]
---

# Docs: rewrite the connection model around CREATE CONNECTION

Part of EPIC `20260630004100`. Flip the docs once declarations are the real path.

## Implementation steps

1. **`docs/guide/connections.md`** — lead with the declaration model: `CREATE CONNECTION … DRIVER …
   AT … SECRET env:/vault:` in a `connections.qfs`, loaded by `serve`/`job`/`run --config`. Keep the
   vault (`qfs connection add` + `QFS_PASSPHRASE`) as the secret *store* behind `SECRET vault:…`.
   Replace the "Two kinds of connection" env-var section with the declaration + secret-ref model;
   note the env vars are deprecated (point at `qfs connection import-env`).
2. **`docs/guide/concepts.md`** — the read-surface table's "Setup" column shows a `CREATE CONNECTION`
   instead of `QFS_SQL_<CONN>=…`; the `<conn>` note links to the declaration page.
3. **Cookbook** (`databases.md`, `code.md`, `cross-service.md`, `automation.md`) — the per-page
   "point `<conn>` at a database" tips use a `CREATE CONNECTION` block; `automation.md` shows
   connections declared in the same `.qfs` config as triggers/policies.
4. Verify every fenced example against the binary; regenerate `docs/language.md`; keep `gen-docs
   --check` green.

## Key files

- `docs/guide/{connections,concepts}.md`, `docs/cookbook/*.md`, `docs/query-cookbook.md`.

## Considerations

- Honesty rule: do this LAST (after `…004150`/`…004170` land) so the docs describe what runs. Until
  then the current env-var docs stay correct.
