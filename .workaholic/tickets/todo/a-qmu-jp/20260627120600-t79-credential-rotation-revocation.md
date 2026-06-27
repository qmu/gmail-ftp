---
created_at: 2026-06-27T12:06:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB]
effort:
commit_hash:
category:
depends_on: [20260626100100-t43-envelope-encryption-sqlite-secret-store.md, 20260626100200-t44-accounts-to-connections-rename.md, 20260626101500-t57-extended-policy-acl.md, 20260626101300-t55-invites-membership.md]
---

# t79 — Credential rotation & revocation (M5)

## Overview

Implements the **rotation/revocation** part of roadmap **decision U** / §4.5 (M5): "member removal
revokes use and **rotates**." A **rotation** re-mints the secret and **re-wraps the data key (DEK)**; a
**revocation** drops the member's `policy` to use the connection. Both land as `/sys/audit` events
(t76). This is the clean answer to "someone left" — the credential they could trigger is *replaced*, not
merely un-granted. Extends t43's single-key envelope (which has no rotation) and t44's connection model;
distinct from t66's M9 OAuth-token refresh.

## Exact seams

- `crates/secrets` (or the t43 envelope store) — a **rotate** operation: accept a new secret via the
  credential-input path, re-encrypt the secret column under a fresh DEK, re-wrap the DEK under the root
  KEK. A secret **never** enters a qfs statement (§4.5) — rotation takes the secret from the CLI prompt
  / admin form, not a query literal.
- The connection model (t44) — a `last_rotated` field; revoke drops the actor/group `policy` (t57) that
  permits *use* of the connection.
- Audit (t76) — emit a rotation event and a revocation event.
- Admin surface (t53) — rotate/revoke actions over `/sys/connections`; CLI `qfs connection rotate`.

## Implementation steps

Each slice leaves the tree green.

1. **Rotate.** Re-mint secret + re-wrap DEK via the credential-input path; set `last_rotated`.
2. **Revoke.** Drop the member's use-policy (t57); leave the connection intact for others.
3. **Audit.** Emit `/sys/audit` events for both (t76).
4. **Surface.** Admin (t53) + CLI; tests that rotation invalidates the prior secret and revocation
   blocks the actor while others keep working.

## Key files

- `crates/secrets` / the t43 envelope store (rotate + re-wrap).
- The connection record (extends [[t44 — accounts→connections rename]]); policy revoke (extends [[t57 — extended POLICY / ACL]]).
- Audit emission ([[t76 — Hash-chained audit event emission]]); admin/CLI ([[t53 — /sys driver + admin views]]).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Secret never in a statement** (§4.5) — rotation reads the new secret from the credential-input path;
  the language sees metadata only.
- **Two-layer identity** (§3.3) — audit records actor + connection for both rotation and revocation.
- **Depends on** [[t43 — envelope encryption / secret store]], [[t44 — accounts→connections rename]], [[t57 — extended POLICY / ACL]], [[t55 — invites / membership]].
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
