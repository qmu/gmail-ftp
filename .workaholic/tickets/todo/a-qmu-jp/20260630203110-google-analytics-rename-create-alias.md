---
created_at: 2026-06-30T20:31:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: 4h
commit_hash:
category: Changed
depends_on: []
---

# Rename `/ga` → `/google-analytics` + a general `CREATE ALIAS` shorthand (owner item #8)

## Owner decision (2026-06-30)

The mount identifier should be the **real (full) name, not a shorthand** — use
**`/google-analytics`** as the canonical mount — and there should be a **general shorthand syntax**
so a user can define a short alias like `/ga`.

## Plan

1. **Rename the GA mount** to `/google-analytics`: `crates/driver-ga/src/path.rs::MOUNT`, the driver
   id sites (`crate::shell` reads/planning, `crate::commit`, `crate::google::consent_scopes`/account),
   and regenerate `docs/drivers.md` (`xtask gen-docs`). Keep `/ga` working as a **deprecated alias**
   for one release.
2. **General `CREATE ALIAS <short> FOR <mount-or-driver>`** (the shorthand mechanism): a new
   declaration (contextual idents — same freeze-safe approach as `CREATE CONNECTION`, commit
   `42d48a3`), parsed into the AST, loaded from `connections.qfs` alongside connections
   (`qfs_core::ddl::connections` is the model), and applied in path resolution so `/ga/...` routes to
   `/google-analytics/...`. The connection name already aliases a source; this aliases a **mount**.
3. The GA *resource* identifier (`/google-analytics/<propertyId>`) stays the **real numeric property
   id** (already correct) — the rename is about the mount word, not the property.

## Key files

- `crates/driver-ga/src/path.rs` (MOUNT), `crates/qfs/src/{shell.rs,commit.rs,google.rs}`,
  `crates/parser/src/{ast.rs,grammar.rs}` + `crates/lang` (the `ALIAS` clause),
  `crates/core/src/resolve.rs` (alias→mount routing), `qfs_core::ddl::connections` (config load),
  `docs/drivers.md` (regenerate).

## Considerations

- Mount rename is a **versioned path-surface change** — additive alias, deprecate `/ga` rather than
  hard-break. The grammar addition must stay additive (no new frozen keyword — contextual idents).
- Decide alias scope: connection-name aliasing already exists; this is mount-level. Keep it general
  (`CREATE ALIAS gh FOR /github`, etc.), not GA-specific.
