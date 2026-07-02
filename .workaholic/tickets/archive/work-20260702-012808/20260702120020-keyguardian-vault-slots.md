---
created_at: 2026-07-02T12:00:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB]
effort:
commit_hash: c6ea563
category: Changed
depends_on: []
---

# KeyGuardian: LUKS-style vault-key slots (passphrase + OS keychain)

Part of EPIC `20260702120000` (ADR 0008 §5). Generalize the single passphrase-wrapped vault DEK
into **N side-by-side wraps of the same DEK** — one per guardian slot — so the passphrase becomes
slot #0 instead of *the* mechanism, and the OS keychain becomes slot #1 (solving the per-pane
re-entry pain). Agent and managed-KMS slots are later work; the seam must make them additive.

## Design

t80 (`e2e_store.rs` / `e2e_recipient_wrap`) already proves the shape: several wraps of one DEK.
Keep the slot **logic pure** (in `qfs-secrets`, next to `envelope.rs` — wasm-buildable, hermetic)
and the keychain/DB **I/O in the binary** (the established pure-decision/IO split).

- **Migration v10**: a `vault_key_slot` table — `(slot_id, guardian_kind, wrapped_dek, kdf_salt?,
  metadata, created_at)`. The existing single-row `secret_meta` (project_secrets.sql, FROZEN)
  becomes read-as-slot-0 or is migrated forward by v10 (copy the wrap into the slot table); choose
  in-ticket, but the ledger stays append-only.
- **Pure slot model** (`packages/qfs/crates/secrets/src/envelope.rs`): `wrap_dek`/`unwrap_dek`/
  `rewrap_dek` (141) already are the primitives; add a slot-set type — unlock = first slot whose
  guardian yields a KEK that unwraps; enroll = wrap the SAME DEK under a new guardian KEK; revoke =
  delete a slot (refuse deleting the last one).
- **Guardians**: `Passphrase` (argon2id derive — today's path, from `resolve_store_passphrase` /
  `PROMPTED_PASSPHRASE` in `connection.rs:75-130`) and `OsKeychain` (Linux Secret Service /
  libsecret via D-Bus; macOS Keychain behind the same trait). On a host with **no secret service
  (headless)**: enroll fails with a clear actionable error, unlock skips the slot silently — never
  a panic, never a hang.
- **CLI**: `qfs vault enroll keychain` / `qfs vault slots` (list kinds + created_at, never key
  material) / `qfs vault revoke <slot>` — a new injected launcher following the qfs-cmd pattern
  (`ConnectionLauncher`@163 as the template). `connection rekey` becomes the passphrase slot's
  rekey (`secret_store.rs::rewrap_passphrase`@174 → slot-scoped).
- **Open path**: `SqliteSecrets::open_or_init` (`secret_store.rs:55`) resolves the DEK through the
  slot set instead of the single `secret_meta` row.

## Key files

- `packages/qfs/crates/secrets/src/envelope.rs` (+ tests 189-329 — extend for slots)
- `packages/qfs/crates/qfs/src/secret_store.rs` (`open_or_init`, `rewrap_passphrase`)
- `packages/qfs/crates/qfs/src/connection.rs` (`resolve_store_passphrase`, `PROMPTED_PASSPHRASE`)
- `packages/qfs/crates/qfs/src/e2e_store.rs` (the N-wrap precedent — mirror, don't merge)
- `packages/qfs/crates/store/src/lib.rs` (v10) + a new `schema/*.sql`
- `packages/qfs/crates/cmd/src/lib.rs` + `crates/qfs/src/main.rs` (the `vault` verb + launcher)

## Considerations

- A keychain dep (e.g. `secret-service`/`keyring` crate) lands **only in the terminal binary** —
  qfs-secrets stays dep-light and wasm-buildable (dep-direction guard).
- This EC2 box is headless: the keychain slot's *graceful-absence* behavior is the locally
  observable case; the working-keychain case is covered hermetically behind a mock guardian.
- No risk framing, no migration shims — but the vault holds real user tokens on this machine, so
  v10's forward-copy of the existing wrap must be covered by a test (old store opens after v10).

## Quality Gate

Global gate (EPIC) plus, per owner decision (keychain in scope):

- Hermetic tests: enroll adds a slot without re-sealing values; unlock succeeds via EITHER slot
  (passphrase removed → keychain still opens, and vice versa); rekey of one slot leaves others
  working; deleting the last slot is refused; wrong passphrase on a multi-slot store still yields
  `Locked` without leaking which slot failed.
- A pre-v10 store (single `secret_meta` row) opens after the migration with its existing
  passphrase — asserted by test.
- On a host without a secret service: `qfs vault enroll keychain` returns an actionable error
  (states the missing dependency), exit code 1, nothing written.
