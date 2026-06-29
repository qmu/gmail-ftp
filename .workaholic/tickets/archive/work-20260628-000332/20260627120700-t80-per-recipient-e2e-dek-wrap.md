---
created_at: 2026-06-27T12:07:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB]
effort: L
commit_hash: da39f86
category: Added
depends_on: [20260626100100-t43-envelope-encryption-sqlite-secret-store.md, 20260626100300-t45-identity-users-accounts-local-signup.md, 20260626101700-t59-selectable-ai-safety-modes.md]
---

# t80 — Per-recipient (E2E) DEK wrap (M5)

## Overview

Implements the **optional end-to-end wrap** of roadmap **decision U** / §4.5 (M5). By default the
**server** can unwrap a connection's data key (DEK) because it must execute the plan (decisions C/F) —
that is the managed-tier trust boundary. For a connection too sensitive for that, the DEK is
**additionally wrapped to individual members' public keys** (registered in `/sys/users`), so it is
decryptable **only** by those members and **not by the server at rest**. The explicit trade-off
(consistent with the safety modes, t59): such a connection **cannot be used by an agent unattended** — a
human with the key must be in the loop. This is the mitigation for the server-compromise threat (§4.5
threat 3). Opposite trust model to t43's server-unwrappable default; this is the opt-in for high
sensitivity.

## Exact seams

- `/sys/users` schema — add a **member public-key** column (extend t45's `(id, primary_email,
  created_at, status)` and the t53 migration); a member registers a keypair (private key stays
  client-side).
- `crates/secrets` (the t43 envelope) — **multi-recipient DEK wrap**: wrap the DEK to each authorized
  member's public key in addition to (or instead of) the root KEK, per the connection's mode.
- The connection record — a flag marking a connection **E2E / unattended-unusable**.
- Safety-mode gating (t59) — an E2E connection is rejected for autonomous agent commit; it requires a
  human unwrap in the loop.

## Implementation steps

Each slice leaves the tree green.

1. **User public keys.** Add the public-key column to `/sys/users` (extend t45/t53 migration); member
   key registration.
2. **Multi-recipient wrap.** Wrap the DEK to each authorized member's public key in `crates/secrets`.
3. **E2E flag + gating.** Mark E2E connections; gate them as unattended-unusable via the safety modes
   (t59).
4. **Tests.** A server without a member key cannot unwrap; an agent commit on an E2E connection is
   refused pending human unwrap.

## Key files

- `/sys/users` schema + migration (extends [[t45 — identity: users/accounts, local signup]] and [[t53 — /sys driver + admin views]]).
- `crates/secrets` multi-recipient wrap (extends [[t43 — envelope encryption / secret store]]).
- Safety-mode gating (extends [[t59 — selectable AI safety modes]]).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Trade-off is the point** — E2E buys server-compromise resistance at the cost of autonomous agent
  use (decision U / J). Make the gate explicit and audited.
- **Depends on** [[t43 — envelope encryption / secret store]], [[t45 — identity: users/accounts, local signup]], [[t59 — selectable AI safety modes]].
- **Distinct from rotation** ([[t79 — Credential rotation & revocation]]) — this is the cryptographic recipient model, not the lifecycle op.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
