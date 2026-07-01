---
created_at: 2026-06-30T20:31:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: 2h
commit_hash: 1eb1ca3
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

## Final Report

**Scoped to Part 1 (the rename) by owner decision** — the ticket bundled two large features and the
general `CREATE ALIAS` grammar touches the versioned/freeze-sensitive grammar surface, so it was
**split into its own ticket** (`20260630204000-create-alias-grammar.md`) for focused design care.
This ticket delivered the mount rename + the built-in deprecated `/ga` shim.

Done here: the GA mount is now `/google-analytics` (the real full name); `/ga` is kept working for
one release as a built-in deprecated alias that parses + routes identically (no hard-break). Docs
regenerated. The internal driver id stays `ga` (see insight) so existing GA connections, the consent
map, and `qfs connection add ga` are untouched — the rename is confined to the user-facing PATH.

### Discovered Insights

- **Insight**: `Driver::id()` DEFAULTS to deriving the runtime driver id from the mount
  (`mount().strip_prefix('/')`). So renaming the mount silently renames the driver id too — which
  keys the read-facet registry, the consent-scope map, and the stored connection selector. Renaming
  it would orphan existing GA connections. The fix is to OVERRIDE `id()` to keep `ga`, confining the
  rename to the path surface.
  **Context**: Any future mount rename of an already-shipped cloud driver must override `id()` (or
  accept a connection migration). The mount (path) and the driver id are only coincidentally equal.
- **Insight**: The docs catalog (`crate::catalog::driver_catalog`) walks `describe_registry()` and
  folds `driver.mount()` + a `representative_path(mount)` map. A renamed mount with no matching
  representative-path arm falls to the mount root, which for GA is the non-describable virtual Root →
  the driver would silently DROP from `docs/drivers.md`. The representative-path map must gain the new
  mount arm. The deprecation alias is registered ONLY in the runtime planning mounts, NOT in
  `describe_registry` — else `driver_catalog` would emit a duplicate GA entry.
