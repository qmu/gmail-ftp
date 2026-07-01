---
created_at: 2026-07-01T10:00:30+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: 4f7a6b0
category: Changed
depends_on: [20260701100010-design-defined-path-model-grammar.md, 20260701100020-defined-path-declaration-grammar-config.md]
---

# Resolution + recursive nesting for `CONNECT`-ed paths

Part of EPIC `20260701100000`. Makes a persisted binding (`100020`) actually **resolve** through the
engine — including recursive `/<folder>/<folder>/<resource>` paths.

## What's already done vs the gap (discovery, 2026-07-01)

- `MountRegistry::resolve_path` (`crates/core/src/registry.rs:421`) **already** longest-prefix-routes
  multi-segment mounts by boundary match — a recursive user path routes with NO router change.
  **Validated** by the landed spike `registry.rs::resolve_path_routes_a_multi_segment_user_mount`.
- Every driver-facing `Path` is rebuilt as `/<driver.id()>/<sub>` (`resolve.rs:622`, `eval.rs`,
  `plan.rs`), so with a canonical `id()` the per-driver parsers keep working untouched.
- **The gap:** `resolve_driver_namespace` (`crates/core/src/resolve.rs:599`) builds a
  **single-segment** `/<namespace>` for `CALL`/pipeline-alias receiver routing — it breaks for a
  multi-segment user path.

## Plan

1. **Multi-segment receiver routing.** Fix `resolve_driver_namespace` (and any single-segment
   `/{namespace}` assumption in `resolve.rs`/`eval.rs`/`plan.rs`) so `CALL`/pipeline-alias resolution
   works on a multi-segment defined path.
2. **Recursive grouping semantics.** A defined path is a multi-segment mount; folders are just longer
   prefixes (`/team` and `/team/finance/ledger` coexist, longest wins). Slot user paths at the
   existing **Mount** precedence tier (`resolve_name`, `registry.rs:195`), never shadowing a realm.
3. **Audit the `id()`-reconstruction sites.** Confirm all four `/<driver.id()>/<sub>` rebuild sites
   behave under multi-segment user paths; add end-to-end coverage for a driver registered under a
   non-canonical multi-segment path (today only the single-segment `/ga` alias exercises this).

## Key files

- `crates/core/src/resolve.rs` (`resolve_driver_namespace:599`, reconstruction `:622`),
  `crates/core/src/registry.rs` (`resolve_path`, `resolve_name`, `peel_scope`),
  `crates/core/src/{eval.rs,plan.rs}`.

## Considerations

- The `register()` realm-shadow guard only checks the FIRST segment — keep it enforcing on user paths.
- Add a route → reconstruct → parse → scan test for a driver under a multi-segment user path.

## Policies

- `design/modeless-design` + `implementation/accessibility-first` (the recursive namespace stays
  reachable by users AND AI agents, no dead paths), `design/access-control` (no escalation via
  unanticipated recursive constructions), `implementation/domain-layer-separation`,
  `implementation/type-driven-design`.
