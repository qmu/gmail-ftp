---
created_at: 2026-07-01T10:00:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort:
commit_hash: f20043a
category: Changed
depends_on: [20260701100000-epic-defined-paths-replace-driver-mounts.md]
---

# Design keystone: the "defined path" model, grammar shape, and the load-bearing decisions

Part of EPIC `20260701100000`. This is the **design-spike / keystone** ticket: it resolves the
open decisions the implementation children (`100020`/`100030`/`100040`) all depend on. The output is
a written design (committed as this ticket's resolution + any short ADR/RFD note), not production code
beyond a throwaway spike if needed to validate a decision.

## Decisions to make

1. **Terminology.** Confirm `defined path` as the user-facing term (the owner's choice). Define the
   one-concept-one-word vocabulary: a *defined path* is a `{ user path → driver + credential }`
   binding; the existing pipeline-verb `alias` (`SEND`/`MERGE`) keeps its name (different layer).
   Record where the word "alias" must be retired (mount alias only): `MountRegistry::register_alias`,
   the `204000` notion, error messages, docs.
2. **Grammar shape (contextual idents — freeze-safe).** Decide the declaration syntax. Candidates:
   extend `CREATE CONNECTION` with a path clause, or a sibling `CREATE PATH`/`DEFINE PATH`. It MUST
   reuse contextual idents (`word(...)`, like `CONNECTION`/`DRIVER`/`SECRET`/`AT`) — **no new frozen
   keyword** (the t31 `AT` lesson). Note `AT` is already taken (locator + policy path clause), so the
   path clause needs a distinct contextual ident. Decide whether the binding is ONE declaration
   (`{path, driver, credential}` together — the owner's "at the same time") or a path clause layered
   onto `CREATE CONNECTION`.
3. **`id()` stays canonical vs parser refactor (LOAD-BEARING).** Decide: keep each driver's
   `id()` canonical (so the `/<driver.id()>/<sub>` reconstruction keeps the per-driver `path.rs`
   parsers working untouched — cheapest, mirrors the `/ga` alias) **vs** refactor parsers to consume
   the registry-supplied sub-path. Recommendation from discovery: keep `id()` canonical. Document the
   consequence: a stored connection's credential is keyed by the canonical driver id, not the user
   path, so the binding table maps `user-path → (driver id, connection)`.
4. **Minimal system-mount set.** Decide which first-paths remain system-defined: the
   `RESERVED_REALMS` set (`members/projects/hosts/directories/me/sys`) + the driver-backed `/sys`,
   and whether `/local` (and `/git`?) stay built-in system mounts or also become user-defined.
   Define the governance rule: a user defined-path may NEVER shadow a realm (the existing
   `register()` guard at `registry.rs:355`).
5. **Recursive nesting semantics.** Define how `/<folder1>/<folder2>/<resource>` resolves: is each
   folder segment part of ONE driver's mount (a multi-segment mount, which `resolve_path` already
   routes), or can folders GROUP multiple defined paths (a true namespace tree)? Decide the
   collision/precedence rules vs `resolve_name` ranking (`Reserved > Lexical(LET) > Mount >
   Connection > Unbound`, `registry.rs:195`) and where user defined-paths slot in.
6. **Deprecate-not-break plan.** Specify the migration: old per-driver mounts (`/github`, `/mail`, …)
   keep routing for one release as deprecated built-in defined-paths (with a warning + a
   `connection`/`path` migration command), then are removed. This is the `rest-api-design`
   deprecate-not-break discipline; cite the `/ga`→`/google-analytics` precedent (ticket `203110`).

## Key files (to ground the design, not necessarily edit here)

- `crates/core/src/registry.rs` (MountRegistry, RESERVED_REALMS, resolve_name, peel_scope),
  `crates/core/src/resolve.rs` (the `/<id()>/<sub>` reconstruction + `resolve_driver_namespace`),
  `crates/driver/src/lib.rs:587` (`id()`/`mount()`), `crates/core/src/ddl/connections.rs`
  (`DeclaredConnection`), `crates/parser/src/{ast.rs,grammar.rs}` (CREATE CONNECTION clauses),
  `README.md` SemVer section.

## Considerations

- Output a crisp written decision for each of the six items above; the implementation children cite
  it. Where a decision is genuinely 50/50 (e.g. `CREATE PATH` vs extend `CREATE CONNECTION`), bring
  it back to the owner rather than guessing — this is the versioned grammar surface.
- A short spike (register a real driver under a user-chosen multi-segment mount with canonical `id()`,
  confirm a query routes + the parser matches) de-risks decision #3 before the children commit to it.

## Policies

- `design/rest-api-design` (deprecate-not-break, surface versioning), `implementation/type-driven-design`
  (additive expression, value-object paths), `design/modeless-design` (namespace reachability),
  `planning/terminology` (alias→defined path), `design/access-control` (the binding is an authz rule).

## Design Resolution — DECIDED (owner, 2026-07-01)

qfs is **experimental: NO backward compatibility, NO migration/deprecation.** A rename or replacement
just removes the old form; do not design deprecation windows, legacy shims, or migration commands.

**The verb is `CONNECT` (a side-effecting statement). There is NO `connections.qfs` file.** A
connection is **server state**, not config — running `CONNECT` mutates the qfs server's own state
(the project DB), exactly like issuing a connection/DDL command to MySQL/Postgres changes server
state. The project DB is the **single source of truth**; `dashboard`/`CLI`/`MCP` all read it
(decision E). This drops the `CREATE CONNECTION`/`CREATE ALIAS` shapes AND the whole
`connections.qfs` config-file concept.

1. **The model.** A **connection** = `{driver + credential}` (can talk to a service). A **defined
   path** = a user path that MOUNTS a connection. One connection can carry MANY paths (aliases).

2. **`CONNECT` — one verb, two arms** (disambiguated by what follows `TO`: a bare driver ident vs a
   leading-`/` path). A read (`/path …`) has no side effect; `CONNECT`/`DISCONNECT` do (commit-class
   effects that mutate server state, on the describe→preview→commit path).
   - **Full connect** (configure connection + mount): `CONNECT /<path> TO <driver> [AT '<locator>']
     [SECRET '<ref>']` — creates the connection keyed by the path and mounts it there.
   - **Alias only** (mount another path onto an existing connection): `CONNECT /<path> TO /<existing-path>`
     — no driver, no secret; resolves to the SAME `(driver-id, connection)`. This is `register_alias`
     made user-expressible.
   - **`DISCONNECT /<path>`** removes a defined path; removing a connection removes its aliases (FK).

3. **`id()` stays canonical.** Each driver's `id()` stays canonical so the `/<driver.id()>/<sub>`
   reconstruction (`resolve.rs:622`, `eval.rs`, `plan.rs`) keeps per-driver `path.rs` parsers
   untouched. The binding maps `user-path → (canonical driver id, connection)`. Proven by the `/ga`
   alias precedent.

4. **Storage (all server state = project DB; no file).** The binding row = metadata (path,
   canonical driver-id, connection, `AT` locator) — NON-secret. The secret VALUE: `SECRET 'env:VAR'`
   stores only the REFERENCE (value read from env at use, never persisted); `SECRET 'vault:…'` is
   sealed envelope-encrypted in the DB's `secret_store`. An unresolvable secret ⇒ the path is defined
   but **fail-closed** (reading it errors "not connected"), never a fake mount.

5. **Minimal system set.** Built-in first-paths = `RESERVED_REALMS`
   (`members/projects/hosts/directories/me/sys`) + driver-backed `/sys` (+ likely `/local`). NOTHING
   else is pre-mounted — `postgres`/`gmail`/`s3`/… are reachable ONLY after a `CONNECT`. A defined
   path may NEVER shadow a realm (`register()` guard). The old fixed per-driver mounts
   (`/github`/`/mail`/…) are simply **removed** (no deprecation — experimental).

6. **Recursive nesting — VALIDATED.** A defined path is a multi-segment mount
   (`/<folder>/<folder>/<resource>`); folders are just longer prefixes. Spike
   `registry.rs::resolve_path_routes_a_multi_segment_user_mount` proves the existing longest-prefix
   router handles them with NO change. Precedence: the existing **Mount** tier (`resolve_name`).

**Open sub-decision (non-blocking):** the token between path and target — `TO` (proposed) vs
`AS`/`=`/`USING`. Locking `TO` unless the owner prefers otherwise.

**Children now proceed on this basis:** `100020` builds the `CONNECT`/`DISCONNECT` grammar + writes
the binding to the DB (no file); `100030` wires resolution (multi-segment already validated);
`100040` removes the fixed per-driver mounts + registers from the DB bindings (no deprecation).
