---
created_at: 2026-06-27T12:08:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB]
effort: L
commit_hash: 8989c14
category: Added
depends_on: [20260626100100-t43-envelope-encryption-sqlite-secret-store.md, 20260626100200-t44-accounts-to-connections-rename.md, 20260626101500-t57-extended-policy-acl.md, 20260626103100-t71-path-expression-scope-realms.md]
---

# t81 — Self-hosted team-shared connections (M5)

## Overview

Implements the **team credential sharing** promised in the **M5** row and roadmap **decision U** / §3.3
for **self-hosted** teams: a **project/team-owned connection** (`owner_scope = project`) that members
use *as the team*, bounded by **actor-based `policy`** — not by who holds a token. The secret is added
once and never re-shared; the audit row records **both** the actor (the human) and the connection (the
credential the effect rode), so "the team acted" traces to one person (§3.3 two-layer identity). This is
the **non-cloud subset** of t66 — t66 delivers the *managed-tier* team connections at M9; today nothing
makes a connection project-owned for a self-hosted team, so M5's promise has no owner without this.

## Exact seams

- The connection record (t44) — an `owner_scope` (`me` vs `project`) so a connection can be team-owned;
  resolution at the project scope.
- `policy` (t57) — evaluates against the **actor** (and groups), never the connection; the connection
  only selects which upstream credential the allowed effect uses.
- Path scope/realms (t71) — `/projects/<proj>/…` resolves to the project's connections.
- Admin/CLI (t53) — add a connection at the project level; list project connections (metadata only).

## Implementation steps

Each slice leaves the tree green.

1. **Project-owned connection.** Add `owner_scope=project` to the connection record (t44); store under
   the project DB.
2. **Actor-policy resolution.** A plan through a project connection is gated by the actor's `policy`
   (t57); the connection picks the upstream credential.
3. **Path scope.** `/projects/<proj>/…` resolves to project connections (t71).
4. **Surface + audit.** Admin/CLI add at project level; audit records actor + connection; tests that two
   members with different policies get different reach over the same shared connection.

## Key files

- The connection record `owner_scope` (extends [[t44 — accounts→connections rename]]).
- Actor-based `policy` resolution (extends [[t57 — extended POLICY / ACL]]).
- `/projects/<proj>` scope ([[t71 — path expression / scope / realms]]); admin/CLI ([[t53 — /sys driver + admin views]]).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **M5 self-hosted, distinct from M9 cloud.** This is the project-owned shared-connection row for a
  self-hosted team; [[t66 — cloud OAuth brokering / team connections]] is the managed-tier (M9) brokering. Keep them separate; t66 can build on this.
- **Policy gates the actor, connection picks the credential** (§3.3) — never policy-on-the-connection.
- **Depends on** [[t43 — envelope encryption / secret store]], [[t44 — accounts→connections rename]], [[t57 — extended POLICY / ACL]], [[t71 — path expression / scope / realms]].
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
