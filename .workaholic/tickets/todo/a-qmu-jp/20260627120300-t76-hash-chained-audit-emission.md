---
created_at: 2026-06-27T12:03:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB]
effort:
commit_hash:
category:
depends_on: [20260626100000-t42-persistence-sqlite-system-project-db.md]
---

# t76 — Hash-chained audit event emission (M0)

## Overview

Implements the **M0** slice of roadmap **decision V** / §4.6: a **hash-chained audit event stream**.
Each audit event records `actor, connection, verb, path, committed, ts` and carries a
`hash(content + prev_hash)`, so any edit, reorder, or deletion at the destination breaks the chain and
is detectable by recomputation. qfs **emits** the stream; it does **not** retain it — `/sys/audit` is a
**live view**, and the durable store + retention are the consumer's concern (§4.6). The only audit
state qfs persists is the **chain head** (to continue the chain). This sits in **M0**, earlier than the
M6 language work, because the commit path must emit chained events from day one.

## Exact seams

- The engine commit/effect path — emit one audit event per committed effect (and per attempted
  irreversible effect), with the canonical content fields.
- System DB schema (extend t42's migration) — a chain-head row (`seq`, `content_hash`, `prev_hash`)
  and the live-view buffer backing `/sys/audit`; chain head persisted, not the whole log.
- A `hash(content + prev_hash)` helper (stable canonical serialization of the event content) + a
  recompute/verify helper for tests and for the consumer-side check (t78).
- `/sys/audit` live view — exposed via the SysDriver (lands fully with t53; this ticket provides the
  emit + chain + head, and the live tail it reads).

## Implementation steps

Each slice leaves the tree green.

1. **Event + content hash.** Define the audit event record and the canonical `content` hash.
2. **Chain + head.** `prev_hash` threading; persist the chain head in the System DB (extend t42).
3. **Emit on commit.** Wire emission into the engine's commit/effect path; metadata only (no secrets,
   no row data).
4. **Verify helper.** Recompute the chain over a sequence and detect the first divergence (used by
   tests now, and by t78's sealing/verification later).
5. **Live view.** Back `/sys/audit` with the recent buffer (full SysDriver surface in t53).

## Key files

- The engine commit path (effect emission).
- System DB migration + schema (extends [[t42 — persistence: System/Project SQLite]]).
- Audit event + hash/verify helpers + tests.
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Emit, don't store.** Retention/period is the consumer's (decision V); qfs keeps only the chain head.
  Durable storage and sinks are [[t77 — Externalized telemetry: file/stdout/OTel sinks]]; external sealing is [[t78 — Audit-chain sealing to an external WORM/transparency log]].
- **Metadata only.** Never secrets or row data — same boundary `describe` enforces (§3.2/§4.6).
- **M0 placement.** Precedes M6; the chain must exist before any multi-user/audit-review work.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
