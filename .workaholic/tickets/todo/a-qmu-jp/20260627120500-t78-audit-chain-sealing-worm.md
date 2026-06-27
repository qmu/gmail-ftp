---
created_at: 2026-06-27T12:05:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260627120300-t76-hash-chained-audit-emission.md, 20260626101100-t53-sys-driver-admin-views.md]
---

# t78 — Audit-chain sealing to an external WORM / transparency log (M3)

## Overview

Implements the **tamper-evidence** half of roadmap **decision V** / §4.6 (M3): periodically **seal the
audit chain head** (from t76) to an **independent, write-once witness** — S3 Object Lock, a transparency
log, or a signed off-box anchor. A hash chain alone can be re-forged wholesale by whoever controls the
store; sealing the head to an append-only witness *outside* the server means a compromised server cannot
rewrite history without contradicting an anchor it can no longer change. qfs emits the seals; **storing
the witness is the platform's** (qfs Cloud on the managed tier; the operator's own bucket self-hosted).
Verification is a read the **consumer** runs over what *it* stored, compared against the seals qfs
emitted — exposed at `/sys/audit/seals`.

## Exact seams

- A **pluggable seal target** — a WORM/transparency-log adapter trait (S3 Object Lock first; a
  transparency-log / signed-anchor variant behind the same trait).
- **Seal cadence** — periodic seal of the current chain head. Scheduling is externalized (decision M):
  the seal is an invokable unit fired by OS cron / Cloudflare Cron Triggers, or a simple periodic emit;
  qfs owns the *what*, not the *when*.
- `/sys/audit/seals` — a SysDriver node (extends t53) exposing emitted seals (`range`, `chain_head`,
  `anchor`, `sealed_at`) so a consumer can verify its own store.
- The recompute/verify helper from t76 — reused for the consumer-side integrity check.

## Implementation steps

Each slice leaves the tree green.

1. **Seal record + target trait.** Define the seal record; a pluggable WORM target (S3 Object Lock).
2. **Seal emission.** Seal the chain head on the externally-fired cadence; record the seal.
3. **`/sys/audit/seals`.** Expose emitted seals via the SysDriver (t53).
4. **Verification.** A consumer-side check: recompute the stored chain and compare to the seals; surface
   the first divergence.

## Key files

- WORM/transparency-log adapter (trait + S3 Object Lock impl).
- SysDriver `/sys/audit/seals` (extends [[t53 — /sys driver + admin views]]).
- Reuses the chain/verify helpers from [[t76 — Hash-chained audit event emission]].
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **The witness is the platform's** (decision V) — qfs seals; it does not run the WORM store.
- **Scheduling externalized** (decision M) — the seal cadence is fired by cron / Cron Triggers, not a
  qfs-internal scheduler.
- **Distinct from the live view and the chain** — this is consumer-side tamper *verification* against an
  independent anchor; depends on [[t76 — Hash-chained audit event emission]].
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
