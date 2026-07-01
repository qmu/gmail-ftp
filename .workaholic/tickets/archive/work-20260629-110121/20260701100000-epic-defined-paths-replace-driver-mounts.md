---
created_at: 2026-07-01T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX, Infrastructure]
effort:
commit_hash: 02c8e8a
category: Changed
depends_on: []
---

# EPIC: `CONNECT` — user-defined paths replace pre-designated driver mounts

## The goal (owner, 2026-07-01)

Today every driver hardcodes a fixed first-path mount (`/ga`, `/github`, `/mail`, `/slack`, `/s3`, …).
Invert this:

1. **Only a MINIMUM set of SYSTEM first-paths stay built in** — the reserved realms + the
   driver-backed `/sys` (and likely `/local`). No third-party driver is pre-mounted.
2. **Everything else is reached only via `CONNECT`.** A user binds a chosen path to a driver +
   credential. `CONNECT` is a **side-effecting statement**: running it mutates the qfs **server's own
   state** (the project DB), exactly like a connection/DDL statement changes a MySQL/Postgres server.
   The DB is the **single source of truth** — there is **no `connections.qfs` config file**.
3. **Recursive / nested paths.** A user can define `/<folder>/<resource>` or
   `/<folder>/<folder>/<resource>` — the path namespace is a user-shaped tree, not a flat driver list.

**qfs is experimental: NO backward compatibility, NO deprecation.** The old fixed per-driver mounts
are simply removed — no shims, no migration window.

## The model

- A **connection** = `{driver + credential}` — the thing that can talk to a service.
- A **defined path** = a user path that MOUNTS a connection. One connection can carry MANY paths.
- `CONNECT` establishes both; `DISCONNECT` removes a path.

```
CONNECT /work/orders TO postgres AT 'postgres://db/orders' SECRET 'env:PG_PASS'   -- configure + mount
CONNECT /db          TO /work/orders                                              -- alias only (reuse)
DISCONNECT /db
```

`TO <driver>` (bare ident) = full connect; `TO /<path>` (leading slash) = alias. A read (`/path …`)
has NO side effect; `CONNECT`/`DISCONNECT` DO (commit-class effects on the describe→preview→commit
path).

## Why this is mostly wiring, not a rewrite (discovery, 2026-07-01)

- Every driver-facing `Path` is reconstructed as `/<driver.id()>/<sub>` before it reaches the driver
  (`resolve.rs:622`, `eval.rs`, `plan.rs`), so a driver's parser only ever sees its **canonical**
  prefix. Keeping `Driver::id()` canonical means the per-driver `path.rs` parsers work **untouched**
  under a user path (proven by the `/ga` alias).
- `MountRegistry::resolve_path` **already routes multi-segment mounts** (longest-prefix boundary) —
  recursive paths route with no router change. **Validated** by the landed spike
  `registry.rs::resolve_path_routes_a_multi_segment_user_mount`.
- `MountRegistry::register_alias` is the routing seam a defined path plugs into.
- `RESERVED_REALMS` is the existing closed system-first-path set (the "minimal system mounts"
  precedent); a defined path may NEVER shadow a realm.

## Sub-tickets (in work order)

1. `20260701100010` — **Design keystone.** DECIDED (owner) — see its Design Resolution (authoritative).
2. `20260701100020` — **`CONNECT`/`DISCONNECT` grammar + persistence.** The statement + the DB binding.
3. `20260701100030` — **Resolution + recursive nesting.** Route the DB bindings through the engine.
4. `20260701100040` — **Registration redesign.** Remove the fixed per-driver mounts; register from the
   DB bindings + the minimal system set.

## Considerations

- **Grammar is additive by contextual ident** (like `SECRET`/`AT`), so `CONNECT`/`DISCONNECT` add no
  frozen keyword — a matter of clean grammar, NOT backward compatibility.
- **Fail-closed:** a path whose credential cannot be resolved is defined but unresolvable — reading it
  errors "not connected", never a fake mount.
- **Anti-drift docs:** the mount model renders into `docs/drivers.md` via `xtask gen-docs`; keep
  `gen-docs --check` green each slice.

## Policies

- `design/access-control`, `design/data-sovereignty`, `design/defense-in-depth` — `CONNECT` binds a
  credential to a path (an authz + secret-handling operation); least-privilege, no escalation via
  recursive path constructions.
- `design/modeless-design`, `implementation/accessibility-first` — the user-defined recursive
  namespace stays reachable without modes, by users AND AI agents.
- `implementation/type-driven-design` — paths + bindings as value objects / sum types, not strings.
- `implementation/domain-layer-separation`, `implementation/persistence` — resolution + the binding
  store are domain/schema logic in qfs's own DB (the single source of truth).
- `implementation/directory-structure`, `implementation/coding-standards` — always apply.
