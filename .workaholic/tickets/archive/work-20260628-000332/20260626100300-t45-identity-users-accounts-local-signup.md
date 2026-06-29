---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort: L
commit_hash: 39ecf20
category: Added
depends_on: [20260626100000-t42-persistence-sqlite-system-project-db.md]
---

# t45 — `users` + `accounts` identity tables + local sign-up

## Overview
Delivers the first real notion of *who a human is* in qfs: System-DB `users` and `accounts`
tables plus a minimal local sign-up (email + argon2id password hash) and a `whoami` path. This
is roadmap **M1** (Identity store) and implements **decision B** (every deployment holds its own
`users` + `accounts` in SQLite) and **§4.1** ("identity is not authorization" — this ticket is
*only* authentication; the OAuth/OIDC authorization surface is M2, t48). The word `accounts` is
now free for human identity because t44 renamed the credential concept to `connections`; here
`accounts` means a *linked sign-in identity* (local password today, an OAuth/OIDC subject later),
many-to-one against a `users` row. **What already exists:** the argon2id KDF (used by the file
vault `from_passphrase` in `crates/secrets/src/local.rs`) and the System DB + migration runner
from t42. **What is genuinely new:** there is NO user/identity model anywhere in the tree today —
`grep` finds no `users`/`accounts` identity tables, no password verification, no `qfs-identity`
crate. This ticket creates that pure-ish identity core; it exposes nothing user-facing beyond a
minimal sign-up / whoami path and deliberately stops short of sessions (t46) and OAuth (M2).

## Exact seams
- **New crate `qfs-identity`** (pure-ish; no tokio): the identity domain — `User`, `Account`,
  `UserId`, `AccountId` (NOTE: distinct from the credential `crates/secrets/src/key.rs`
  `AccountId`/`CredentialKey`, which t44 renames to `ConnectionId`; the identity `AccountId` is a
  *new* type for human sign-in identities, not service credentials). Sign-up validation and the
  store trait live here; SQLite I/O is injected.
- `crates/store` (the `qfs-store`/`qfs-persist` crate from **t42**) — the System DB. This ticket
  adds an `0002_identity` migration to t42's embedded, versioned migration runner and a
  `schema_version` bump; `users`/`accounts` are System-DB tables (per host, decision B / §4.2).
- `crates/crypto-core/src/lib.rs` — pure leaf (`sha256`, `hmac_sha256`, `constant_time_eq`,
  `sha256_hex`, `hex_lower`, ZERO deps). Password verification MUST use a constant-time compare;
  the argon2id hashing reuses the same argon2id dependency `crates/secrets` already vendors for
  `LocalStore::from_passphrase` (do NOT add a second password-hash crate). Keep `crypto-core`
  dependency-free — argon2id lives with secrets/identity, not in `crypto-core`.
- `crates/secrets/src/secret.rs` `Secret` — the redacted/zeroized wrapper. A plaintext password
  submitted at sign-up MUST be carried as `Secret` and zeroized after hashing; never `String`.
- `crates/cmd/src/lib.rs` — CLI surface. Add a new identity verb group (mirroring the existing
  `enum AccountVerb { Add, List, Use, Remove }` / `AccountAction` injection pattern) for
  `qfs identity signup` / `qfs identity whoami`, with the live store injected as a closure.
- `crates/qfs/src/main.rs` — composition root; injects the System-DB-backed identity store
  closure into `qfs_cmd::run(...)` (the same injection seam the 7 driver/account closures use).
- `crates/cmd/tests/dep_direction.rs` — `qfs-identity` is a new crate; add it to the appropriate
  allowlist. It must NOT depend on lang/plan/driver/codec/parser; tokio stays out of it.

## Implementation steps
1. **Migration + schema (tree stays green).** Add an `0002_identity` migration to t42's runner
   creating `users (id, primary_email, created_at, status)` and `accounts (id, user_id,
   provider, subject, password_hash NULLABLE, created_at)` with a unique index on
   `(provider, subject)` and on `users.primary_email`. `provider='local'` rows carry an argon2id
   `password_hash`; OAuth/OIDC providers (t49, t56) leave it null. No rows yet; assert the
   migration applies idempotently on a fresh and an already-migrated System DB.
2. **`qfs-identity` domain core.** Define `User`/`Account`/`UserId`/`AccountId` owned types and a
   `trait IdentityStore { create_user, find_user_by_email, create_account, find_account,
   verify_password }` (consumer-side trait; SQLite impl injected). Implement sign-up validation
   (email shape, password length/policy) as pure functions with unit tests.
3. **Password hashing.** Implement `hash_password(Secret) -> PasswordHash` (argon2id, the
   `crates/secrets` dependency) and `verify_password(Secret, &PasswordHash) -> bool` using a
   constant-time compare; zeroize the plaintext `Secret` after use. Pure/unit-testable with a
   fixed-salt fixture (no I/O).
4. **SQLite store impl.** Implement `IdentityStore` over the System DB (rusqlite, sync — no
   tokio). Sign-up = one transaction inserting a `users` row + a `local` `accounts` row;
   duplicate email/subject returns a structured, machine-legible error (no panic). Keep this in
   the binary-injected layer per dep-direction.
5. **CLI wiring.** Add the `identity signup`/`identity whoami` verbs in `crates/cmd` and inject
   the store from `crates/qfs/src/main.rs`. `whoami` prints the current user's email + user id
   only (never a hash). Add `qfs-identity` to `dep_direction.rs` allowlists.
6. **Docs honesty + version.** Do NOT advertise sign-in/login in README/skill/roadmap status
   tags yet (sessions land in t46, real auth in M2); at most note "local sign-up exists, no
   session yet". Run `cargo run -p xtask -- gen-docs --check` (CLI verb surface may need a
   regen). Bump the patch in `crates/qfs/Cargo.toml` (0.0.7 → next).

## Key files
- `crates/identity/` (new): `Cargo.toml`, `src/lib.rs`, `src/model.rs` (`User`/`Account`/ids),
  `src/store.rs` (`IdentityStore` trait), `src/signup.rs` (validation), `src/password.rs`
  (argon2id hash/verify).
- `crates/store/src/migrations/0002_identity.sql` (or the runner's embedded-migration form from
  t42) + `schema_version` bump.
- `crates/store/src/identity_store.rs` (new): the rusqlite `IdentityStore` impl over the System DB.
- `crates/cmd/src/lib.rs` (modify): identity verb group + injected launcher.
- `crates/qfs/src/main.rs` (modify): inject the System-DB identity store closure.
- `crates/cmd/tests/dep_direction.rs` (modify): allowlist `qfs-identity`.
- `crates/qfs/Cargo.toml` (modify): patch bump.

## Considerations
- **Safety floor.** Sign-up is a write but a *new-row* one (reversible); it is not an irreversible
  effect and needs no `--commit-irreversible`. There is no policy/authorization here yet — keep
  that boundary crisp (§4.1): this ticket answers "who are you", never "what may you do". Flag in
  the ticket that until M2, a signed-up user can DO nothing privileged — that is intentional.
- **Secret hygiene.** The plaintext password is `Secret`, zeroized after hashing; `password_hash`
  is never logged, never returned by `whoami`, never serialized into audit. Password verification
  is constant-time (`crypto-core::constant_time_eq`). Argon2id parameters are pinned and recorded
  in the hash string so a future cost bump can re-hash on next login.
- **Dep-direction.** `qfs-identity` is a pure-ish domain leaf — no tokio, no driver/lang/plan/
  codec deps. SQLite I/O (rusqlite, sync) lives in the binary-injected `crates/store` layer, not
  in `qfs-identity`. Add the new crate to `dep_direction.rs` allowlists; live wiring lands only on
  `crates/qfs`.
- **Identity `AccountId` ≠ credential `ConnectionId`.** Two different concepts now share neither
  name nor table after t44/t45. Name the identity type unambiguously and document the distinction
  in the crate docs so no future ticket conflates a sign-in identity with a service credential.
- **Open product decisions to flag (do not guess).** (a) Password policy / minimum strength and
  whether to allow passwordless-only deployments. (b) Whether a single `users` row may own
  multiple `local` accounts or just one — keep the schema permissive (account→user is many-to-one)
  but pick a sign-up default. (c) Email verification on sign-up is deferred to M5 invites (t55) —
  note it, do not build it here.
- **Versioning.** One PR, one patch bump, a `v0.0.x` tag on ship, per CLAUDE.md.
