---
created_at: 2026-07-01T10:00:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort:
commit_hash:
category: Added
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

## Design Resolution — PROPOSED (night run 2026-07-01; ONE owner decision outstanding)

Drafted autonomously during a `night /drive` for morning review. Five of the six decisions have a
clear recommendation grounded in discovery; **decision #2 (grammar syntax) is genuinely the owner's
call** and is why this ticket stays in `todo` (NOT archived) pending sign-off.

1. **Terminology — DECIDED.** `defined path` (owner's term) for a `{user path → driver + credential}`
   binding. The pipeline-verb `alias` (`SEND`/`MERGE`) keeps its name (different layer). Retire
   "alias" only for the mount sense: rename `MountRegistry::register_alias` → `register_defined_path`,
   update error messages + docs, in the same change (`planning/terminology`).

2. **Grammar syntax — OPEN, OWNER DECISION.** Two viable shapes, both freeze-safe (contextual idents,
   no new keyword):
   - **(A) One declaration — extend `CREATE CONNECTION` with a path clause** (recommended). Best fits
     the owner's "define the path AND the credential at the same time": one statement carries
     `{name, driver, secret, PATH '<…>'}`. The path clause needs a NEW contextual ident (`AT` is
     taken by the locator + the policy path clause) — proposed word: `PATH` (or `MOUNT`/`AS PATH`).
   - **(B) A sibling `CREATE PATH '<…>' FOR <connection>`** — separates path-binding from credential
     config; closer to the abandoned `CREATE ALIAS` shape but renamed.
   Recommendation: **(A)** + the contextual ident word `PATH`. *Owner: confirm A vs B, and the word.*

3. **`id()` stays canonical — DECIDED (recommended).** Keep each driver's `id()` canonical so the
   `/<driver.id()>/<sub>` reconstruction (`resolve.rs:622`, `eval.rs`, `plan.rs`) keeps per-driver
   `path.rs` parsers untouched. The binding table maps `user-path → (canonical driver id, connection)`;
   the credential stays keyed by the canonical driver id (no connection migration). Proven in
   production by the `/ga` alias (canonical id `ga`, non-canonical mount).

4. **Minimal system set — DECIDED (recommended), minor confirm.** System-defined first-paths =
   `RESERVED_REALMS` (`members/projects/hosts/directories/me/sys`) + the driver-backed `/sys`, plus
   keep `/local` (and `/git`?) as built-in system mounts (local-first). Everything else is a user
   defined-path. Governance rule unchanged: a defined-path may NEVER shadow a realm (`register()`
   guard, `registry.rs:355`). *Owner: confirm `/local` + `/git` stay built-in.*

5. **Recursive nesting — DECIDED (recommended), VALIDATED.** A defined path is a **multi-segment
   mount** for v1 (`/<folder>/<folder>/<resource>`); folders-as-grouping-nodes-over-multiple-paths is
   a future extension. **De-risk spike landed:** `registry.rs::resolve_path_routes_a_multi_segment_user_mount`
   proves multi-segment mounts route through the existing longest-prefix router with NO change.
   Precedence: user defined-paths slot at the existing **Mount** tier (`resolve_name`,
   `registry.rs:195`: `Reserved > Lexical > Mount > Connection > Unbound`).

6. **Deprecate-not-break — DECIDED.** Old per-driver mounts (`/github`, `/mail`, …) become built-in
   **deprecated** defined-paths for one release (warning + a `connection`/`path` migration command),
   then removed — mirroring `/ga`→`/google-analytics` (ticket `203110`, `register_alias`).
   (`design/rest-api-design`.)

**Implementation children are UNBLOCKED once the owner signs off #2 (+ the #4 confirm).** With those
settled, `100020` builds the chosen grammar, `100030` wires resolution (the multi-segment premise is
already validated), `100040` does the registration redesign + deprecation.
