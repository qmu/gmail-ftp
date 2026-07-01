---
created_at: 2026-06-30T20:40:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# General `CREATE ALIAS <short> FOR <mount>` shorthand (split from 20260630203110)

> **SUPERSEDED (2026-07-01) — subsumed by EPIC `20260701100000`.** The owner reframed this into a
> broader redesign: third-party drivers are no longer pre-mounted; every one is bound to a
> user-defined **"defined path"** (the renamed "alias") declared together with its credential, plus
> recursive nested paths. The path-binding grammar is now designed ONCE under the new epic (see
> `20260701100020`) rather than as this narrow shorthand. Moved to `abandoned/` as subsumed, not
> failed. Retained for context.

## Context

Split out of `20260630203110` (owner item #8). That ticket's **Part 1 — rename `/ga` →
`/google-analytics` + a built-in deprecated `/ga` path alias — is DONE** (see its archive entry).
This ticket is **Part 2**: the *general, user-defined* mount-alias mechanism. It was deferred to its
own session because it adds a new declaration to the **versioned, freeze-sensitive grammar surface**
(the README's SemVer policy: the versioned surface = grammar + registries), which deserves focused
design care rather than being rushed at the tail of a drive batch.

## What's wanted (owner decision, 2026-06-30)

A general shorthand so a user can define a short alias for a mount: `CREATE ALIAS <short> FOR
<mount>` (e.g. `CREATE ALIAS ga FOR /google-analytics`, `CREATE ALIAS gh FOR /github`). Keep it
**general** (not GA-specific). The connection name already aliases a *source*; this aliases a
**mount**.

## Plan

1. **Grammar (additive, freeze-safe).** Add `CREATE ALIAS <short> FOR <mount>` as a new declaration
   using **contextual idents** (the same freeze-safe approach as `CREATE CONNECTION`, commit
   `42d48a3` — NO new frozen keyword). Parse into the AST.
   - Files: `crates/parser/src/{ast.rs,grammar.rs}` + `crates/lang` (mirror the `CREATE CONNECTION`
     nodes/rules), `crates/parser/src/tests.rs`.
2. **Config model + load.** Model the alias declaration alongside connections in
   `qfs_core::ddl::connections` (`crates/core/src/ddl/connections.rs`) and load it from
   `connections.qfs` (the same file connections load from).
3. **Resolution routing.** Apply aliases in path resolution so `/<short>/...` routes to
   `/<mount>/...`. The built-in deprecation shim already added `MountRegistry::register_alias`
   (`crates/core/src/registry.rs`) — a user `CREATE ALIAS` should register through the same seam (or
   a sibling). Decide whether user aliases also surface a deprecation-style note (probably not).

## Key files

- `crates/parser/src/{ast.rs,grammar.rs,tests.rs}`, `crates/lang`,
  `crates/core/src/ddl/connections.rs` (config model + load),
  `crates/core/src/registry.rs` (`register_alias`, already exists),
  `crates/core/src/resolve.rs`, `docs/` (regenerate any grammar/driver docs).

## Considerations

- **Terminology collision (decide):** `crate::resolve` already uses "alias" for **receiver-typed
  pipeline-verb aliases** (`SEND`, `MERGE`) — a *different* concept from a mount alias. The two live
  in different layers (pipeline resolution vs path routing) so they don't collide in code, but the
  WORD is overloaded. Confirm `CREATE ALIAS` (mount alias) is the right surface name, or pick another
  (`CREATE MOUNT ALIAS`, `CREATE SHORTCUT`, …).
- The grammar addition must stay **additive** (contextual idents, no new frozen keyword) — it touches
  the versioned surface, so regenerate docs and keep the cookbook parse-coverage test green.
- The built-in `/ga → /google-analytics` deprecation alias (Part 1) is hardcoded in
  `register_google_planning_mounts`; once `CREATE ALIAS` ships, decide whether to leave it hardcoded
  (it is a deprecation shim, not a user preference) or express it through the new mechanism.
