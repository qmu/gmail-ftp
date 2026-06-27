---
created_at: 2026-06-26T10:01:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260626100000-t42-persistence-sqlite-system-project-db.md]
---

# t43 — Envelope-encrypted credential store on SQLite (scrap the file vault)

## Overview

Part of **M0 — Persistence foundation**, implementing roadmap decision **E**: "credentials
envelope-encrypted at rest … no migration of today's file vault — scrap & build." Today the credential
seam `crates/secrets/src/store.rs` `trait Secrets { get/put/remove/list }` is backed by `LocalStore`
(`crates/secrets/src/local.rs`) — a single encrypted FILE vault at `default_credentials_path()`
(`~/.config/qfs/credentials`) with `credentials.salt`/`credentials.active` sidecars, ChaCha20Poly1305 +
argon2id, blob `MAGIC || nonce || ciphertext` over a JSON `Vault`. This ticket **replaces that file
backend** with a new SQLite-backed `Secrets` impl over the **Project DB** (t42) using **envelope
encryption** (roadmap §4.2): a passphrase / OS keychain unwraps a **data-key**, and the data-key
encrypts the secret columns inside the DB. The `Secrets` trait, `EnvStore`/`WorkerStore`
(`crates/secrets/src/backends.rs`, `worker.rs`), and the `Secret` redaction/zeroization
(`crates/secrets/src/secret.rs`) are **reused unchanged** — only the default at-rest backend is new.
There is deliberately **no data migration** from the old file vault (decision E).

## Exact seams

- `crates/secrets/src/store.rs` — `pub trait Secrets { get/put/remove/list }`. The new SQLite store
  implements this **same trait**; every driver + the server already read through it, so nothing
  downstream changes.
- `crates/secrets/src/local.rs` — `LocalStore` (`from_passphrase(path, secret, salt)`,
  `open_with_key(path, key)`, ChaCha20Poly1305 + argon2id, `Vault` JSON). This file backend is the
  thing being **scrapped as the default**; the new store reuses its crypto choices (ChaCha20Poly1305
  AEAD, argon2id KDF) but stores ciphertext in DB columns, not a file blob.
- `crates/secrets/src/key.rs` — `AccountId`, `CredentialKey { driver, account }`, `AccountRecord`,
  `DriverId`. The SQLite schema is keyed by `(driver, account)` exactly as `CredentialKey` already
  models; **no rename here** (the rename to `connections` is t44, which depends on this).
- `crates/secrets/src/secret.rs` — `Secret` (redacted `Debug`/`Display`, zeroized on drop). Preserved
  verbatim; the new store decrypts INTO a `Secret` and never widens its surface.
- `crates/secrets/src/active.rs` / `resolve.rs` — `ActiveAccounts`, `resolve()` ladder,
  `AccountSource`, `Resolution`. The active-selection moves from the `credentials.active` sidecar into
  a Project-DB table; the `resolve()` precedence is unchanged.
- `crates/crypto-core/src/lib.rs` — pure leaf (`sha256`, `hmac_sha256`, `constant_time_eq`,
  `sha256_hex`, `hex_lower`, ZERO deps). The **envelope data-key wrap/unwrap** extends here or a pure
  sibling crate — the AEAD primitive must live in a pure, wasm-buildable leaf, NOT pull tokio.
- `crates/store/` (t42) — the Project DB `Db`/`ProjectDb` handle + migration runner; this ticket adds
  a migration creating the secret columns (ciphertext + nonce + wrapped-data-key metadata).
- Binary wiring: `crates/qfs/src/account.rs` (`open_store()`, `open_store_for_commit()`,
  `run_account`, currently constructs `LocalStore::from_passphrase`) and `crates/qfs/src/commit.rs`
  (`networked_credential(driver)`, `live_registry()`) — both swap the concrete backend from
  `LocalStore` to the new SQLite store. `crates/core/src/lib.rs` re-export stays the same.

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` +
`cargo build --target wasm32-unknown-unknown` for the pure crates + `cargo run -p xtask -- gen-docs --check`).

1. **Data-key envelope primitive (pure).** Extend `crates/crypto-core` (or a pure sibling) with AEAD
   data-key generation + wrap/unwrap: a random data-key encrypts the secret columns; the data-key
   itself is wrapped by a key-encryption-key derived from the passphrase (argon2id, as `local.rs`
   does today). Keep it I/O-free and wasm-buildable. Green: round-trip + tamper-detection unit tests,
   no network/credentials.
2. **SQLite `Secrets` backend.** Add a Project-DB migration (t42 runner) for the secret store schema
   keyed by `(driver, account)` (`CredentialKey`) with ciphertext/nonce columns + a wrapped-data-key
   row. Implement the `Secrets` trait over `ProjectDb`: `put` encrypts, `get` decrypts into a
   `Secret`, `remove`/`list` operate on rows (`list` returns `AccountRecord` metadata, never
   plaintext). Green: `put → get → remove` round-trip on a `:memory:` DB.
3. **Move active-selection into the DB.** Port `ActiveAccounts` from the `credentials.active` sidecar
   to a Project-DB table; keep `resolve()`/`AccountSource`/`Resolution` semantics identical. Green:
   the existing resolution golden tests pass against the DB-backed active store.
4. **Swap the default backend in the binary.** Change `crates/qfs/src/account.rs` `open_store()` /
   `open_store_for_commit()` and `crates/qfs/src/commit.rs` `networked_credential()` to construct the
   SQLite store (unlocked via `QFS_PASSPHRASE`, as `LocalStore` is today) instead of `LocalStore`.
   `EnvStore`/`WorkerStore` remain available as alternative backends. Green: `qfs account
   add/list/use/remove` round-trips against the Project DB; commit path resolves a live credential.
5. **Retire the file vault as default + honest docs.** Stop the binary from creating the file vault;
   `LocalStore` may stay in `crates/secrets` as code but is no longer the default backend. Update
   `crates/skill/assets/SKILL.md` / README / `docs/roadmap.md` status only for what now works. Green:
   `gen-docs --check` clean; no doc claims a capability beyond this slice.

## Key files

- `crates/crypto-core/src/lib.rs` (modify) or new pure sibling: AEAD data-key wrap/unwrap.
- `crates/secrets/src/` (new file, e.g. `sqlite.rs`): the `Secrets` impl over `ProjectDb`; wire into
  `lib.rs`. `active.rs` (modify): DB-backed `ActiveAccounts`.
- `crates/store/src/migrate.rs` / schema (modify): secret-store + active-selection tables.
- `crates/qfs/src/account.rs`, `crates/qfs/src/commit.rs` (modify): construct the SQLite store.
- `crates/qfs/Cargo.toml` version bump (next patch).

## Considerations

- **Safety floor.** A credential is **not an effect** in a `Plan` — resolution happens at COMMIT time
  when a driver leg binds to a live client (the §3 purity invariant: a `Plan` embeds an account
  *selector*, never a secret). This ticket must preserve that: decryption is a `Secrets::get` at bind
  time, downstream of preview.
- **Redaction is non-negotiable.** `Secret` keeps its redacting `Debug`/`Display` + zeroize-on-drop;
  the new store decrypts straight into `Secret` and never logs ciphertext, the data-key, or the KEK.
  A `format!`/`tracing` call must never take a `Secret` or a raw key — keep the existing grep/clippy
  guard green.
- **Dep-direction discipline.** The AEAD data-key primitive lives in a **pure, wasm-buildable** leaf
  (`crypto-core` or sibling) so `crates/secrets` stays I/O-light and the binary leaf is the only place
  that opens the real Project DB (via t42's seam). No tokio in `crypto-core`/`secrets`/`store`.
- **No migration (decision E).** Do NOT write a file-vault→DB importer. The project is experimental;
  the old vault is scrapped. Document the one-time re-`account add` for existing users in the release
  note, not a migration path.
- **Open product decision to FLAG, not guess: passphrase source UX.** Today unlock is purely
  `QFS_PASSPHRASE`. Envelope encryption invites an **OS-keychain** path (Keychain / secret-service /
  Credential Manager) to unwrap the KEK without an env var. Which sources ship, their precedence, and
  the fallback story is an open UX decision — flag it for the reviewer; ship the `QFS_PASSPHRASE` path
  first (parity with `local.rs`) and leave keychain as a documented follow-up seam.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag on ship.
