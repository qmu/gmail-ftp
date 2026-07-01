---
created_at: 2026-07-01T10:00:40+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure, UX]
effort:
commit_hash: 81d1a30
category: Changed
depends_on: [20260701100010-design-defined-path-model-grammar.md, 20260701100020-defined-path-declaration-grammar-config.md, 20260701100030-recursive-path-resolution-nesting.md]
---

# Registration redesign: minimal system mounts + register from the DB bindings (remove fixed mounts)

Part of EPIC `20260701100000`. The final slice: stop hardcoding per-driver mounts. Register only the
minimal system set + each `CONNECT`-ed path from the DB bindings. **qfs is experimental — the old
fixed per-driver mounts are simply REMOVED, no deprecation, no shim.**

## The three static sites to redesign (discovery, 2026-07-01)

Every driver mount is wired statically at three places; all switch from "bulk register under each
driver's `const MOUNT`" to "register the minimal system set + loop over the DB bindings, registering
each driver under its user path with `id()` canonical":

1. `crates/qfs/src/shell.rs` (`run_engine_and_reads:177`, `register_google_planning_mounts:153`) —
   planning + read facets.
2. `crates/qfs/src/describe.rs` (`describe_registry:53`) — the cred-free DESCRIBE/catalog registry;
   must register the bindings or DESCRIBE won't surface user paths
   (`all_registered_mounts_describe_cred_free:192` re-founds on the bindings).
3. `crates/qfs/src/commit.rs` (`register_google:450`, `register_objstore:363`) — the apply registry;
   the connection's credential drives the user-mount registration.

## Plan

1. **Minimal system set.** Register only the keystone-decided built-ins (realms are not mounts;
   `/sys` + `/local`, possibly `/git`).
2. **Register from the DB bindings.** For each `CONNECT`-ed binding (`100020`), construct the driver
   instance + `register`/`register_alias` it under the user path, keeping `id()` canonical so the
   reconstruction sites + parsers (`100030`) keep working. Generalize the existing "declared
   connection → built mount" seam (today `/sql`, `/git`) to ALL drivers, connection AS the mount.
   Preserve **fail-closed**: an unresolvable binding leaves the driver UNREGISTERED with a clear error.
3. **Remove the fixed per-driver mounts.** Delete the hardcoded `/github`/`/mail`/`/drive`/`/slack`/
   `/s3`/`/ga`(→google-analytics)/… bulk registrations and the per-driver `MOUNT` consts' use as
   auto-mounts. No deprecation alias, no warning, no migration — they're gone; reach them via
   `CONNECT`.
4. **Docs.** Regenerate `docs/drivers.md` (the catalogue now reflects the bindings); `gen-docs
   --check` green; bump the patch version.

## Key files

- `crates/qfs/src/{shell.rs,describe.rs,commit.rs}`, `crates/qfs/src/{sql.rs,connection.rs}` (the
  binding→registry seam), `crates/core/src/registry.rs`, `crates/qfs/src/catalog.rs`
  (`representative_path` / `driver_catalog` for docs).

## Considerations

- Highest-blast-radius slice: it changes how EVERY driver mounts. Land it after `100020`+`100030` so
  a `CONNECT`-ed path is fully resolvable before the fixed mounts are removed.
- `describe_registry` registering a driver under multiple keys double-counts it in `driver_catalog`
  (the `203110` insight) — register the user path for docs, aliases as routing-only.
- Re-found `all_registered_mounts_describe_cred_free` and any per-driver `mount_and_id_are_X` test on
  the new model.

## Policies

- `implementation/domain-layer-separation` (the minimal-set + binding loop is domain logic, not CLI
  glue), `implementation/persistence` (registration is founded on the persisted binding store),
  `design/access-control`/`design/data-sovereignty` (a binding registers a credentialed path),
  `implementation/accessibility-first` (DESCRIBE surfaces user paths for AI agents).
