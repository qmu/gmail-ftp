---
created_at: 2026-06-26T10:31:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260626100200-t44-accounts-to-connections-rename.md]
---

# t71 — Path expression: scope realms + reserved-name resolution (decision P)

## Overview

Implements roadmap **decision P / §1.3**: a path names three axes — **scope** (whose), **service**
(what), **coordinate** (when) — and gains an explicit **scope** prefix so a path says *whose* resource
it is, not just *what*. Root becomes a **closed, reserved set of realms** — `/members`, `/projects`,
`/hosts`, `/directories` (plural collections) plus the singletons `/me` and `/sys` — and within a
scope, connections/accounts are **plain `/`-segments** (no new punctuation). This is mostly a
**name-resolution** change, not a lexer/grammar one: a path is still `/`-segments today, so the lexer
and AST are unchanged; what is new is (a) recognizing realm-led scopes, (b) the two disambiguation
rules that keep it unambiguous, and (c) threading the resolved scope/connection into credential
resolution and `POLICY`. The flat form (`/sql/pg/orders`, `/git/app@v2.1/...`) keeps working — a bare
path is sugar for `/me/...`.

## Exact seams

- `crates/lang/src/lex.rs` `lex_path()` + `crates/parser/src/ast.rs` `PathExpr`/`PathSegment` —
  **unchanged**: a path lexes/parses as segments (+ per-segment `@version`, globs) exactly as today.
  Scope is a resolution concept layered on the same segment list, not new grammar.
- `crates/core/src/registry.rs` `MountRegistry::{register, resolve_path}` — the core change. Add a
  **reserved realm set** (`members/projects/hosts/directories/me/sys`); `resolve_path` peels a leading
  scope (a realm + **exactly one** principal segment, or a bare singleton `/me`/`/sys`) before
  longest-prefix-matching the remaining **service** path against driver mounts. `register` gains the
  **governance rule**: a driver mount MUST NOT be named after a realm (reject at registration) — this
  is what makes the scope↔service boundary decidable (the two §1.3 rules: reserved realms + single
  principal arity).
- `crates/core/src/resolve.rs` `Resolver` — thread a resolved `Scope { realm, principal }` (or
  `Me`/`Sys`) through resolution so downstream stages know *whose* world a node lives in; a bare path
  resolves scope = `Me`.
- `crates/secrets/` (`resolve.rs` ladder) + [[t44]] connections — the connection used is now
  **determined by the scope/connection segments in the path**, not an ambient active selection. The
  `<provider>/<account>` pair is the **connection key**, unique within a scope (§1.3): re-consent
  replaces tokens in place; revoke removes the node + every service facet that rode the grant; the
  account label is a local alias, not the upstream email. An ambiguous bare `/gmail` with two accounts
  and no default returns a structured "which account?" error — never a silent pick.
- `crates/server/src/policy/` (t57) — `POLICY` globs match the **scoped** path (`/me/**`,
  `/members/*/gmail/*`); the path is the authorization subject (decision P). Policy evaluates the
  **actor**, the connection only supplies the upstream credential (the two-layer identity from §3.3).
- `crates/qfs/src/describe.rs`/`commit.rs` composition root — wire the realm-aware registry; a path
  with no `POLICY` grant is **invisible** to `describe` (not merely unreadable).

## Implementation steps

1. **Reserved realms + governance.** Add the realm set and the `register`-time check (driver mount
   must not shadow a realm). Unit-test that registering `/members` as a driver is rejected. Tree green.
2. **Scope peeling in `resolve_path`.** Recognize a leading realm + one principal (or `/me`/`/sys`
   singleton), split `(scope, service)`, resolve the service against mounts; a bare path → scope `Me`.
   Golden tests over `/members/alice/gmail/inbox`, `/me/google/work/gmail/inbox`,
   `/hosts/ci/claude/sessions`, `/sys/audit`, `/sql/pg/orders` confirm the boundary is unambiguous.
3. **Connection key + scope-driven credential resolution.** Make the `<provider>/<account>` segments
   select the connection (via [[t44]]'s store), with uniqueness + the "which account?" structured
   error; revoke/re-consent semantics. Reuse the `qfs-secrets` ladder for the *default* account only.
4. **Glob over a collection.** `/members/*/gmail/inbox` (one-level `*` in principal position) fans the
   read across the collection; `**` stays a service-segment recursive glob. Cover both.
5. **Policy + describe-visibility.** Policy globs evaluate on the scoped path; an ungranted subtree is
   invisible to `describe`. Wire and test against the t35/t57 policy engine.

## Key files

- `crates/core/src/registry.rs` (realm set, `register` governance, `resolve_path` scope peel),
  `crates/core/src/resolve.rs` (`Scope` threading).
- `crates/secrets/src/resolve.rs` + the connection store ([[t44]]) — scope/connection-keyed credential
  resolution, uniqueness, revoke/re-consent.
- `crates/server/src/policy/*` (scoped-path globs; actor-vs-connection), `crates/qfs/src/describe.rs`
  (visibility).
- Governance/golden tests for the realm set and the scope↔service boundary.
- `crates/qfs/Cargo.toml` (patch bump).

## Considerations

- **Unambiguous only under the two rules.** Reserved realm names + single principal arity are what
  make `(scope, service)` decidable; encode BOTH as tests, because relaxing either reintroduces the
  ambiguity §1.3 calls out. Segment *meaning inside the service* stays driver-declared (`describe`) —
  the path-is-the-type model, not an ambiguity to resolve here.
- **No new punctuation, no new keyword.** Realms are reserved *path segments*, not keywords; the
  closed-core keyword set is untouched. Adding a realm later is a deliberate governance event (like a
  top-level FS directory), parallel to the keyword freeze.
- **Security.** The path is the authorization subject: `/me/**` is free, `/members/<other>/**` needs a
  grant, and the audit row records the fully-qualified scoped path + the acting human (§3.3). Describe
  visibility is itself an authorization boundary — do not leak structure for ungranted scopes.
- **Foundational ordering.** M4 (cloud connections), M5 (multi-user/policy), and M7 (fabric `/hosts`)
  all assume this addressing; land it before those lean on scoped paths. Depends on [[t44]] (the
  connection model the account segment selects).
- **Versioning:** own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
