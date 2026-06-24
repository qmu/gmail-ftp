---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Config, Infrastructure]
effort:
commit_hash: 0c6a2c2
category: Added
depends_on: [20260622214650-t01-rust-workspace-single-binary-scaffold.md]
---

# Credential / secret store + multi-account resolution

## Overview

`cfs` is one binary holding tokens for Gmail, Drive, S3/R2, D1, GitHub, Slack, AWS,
and Cloudflare while running cross-service effect-plans — a large blast radius
(RFD §10). This ticket delivers the **single secrets surface** that every driver
and the server read from: an encrypted-at-rest credential store keyed by
`(driver, account)`, plus the **account-resolution** that turns a statement's
context into a concrete credential. It implements the credential/least-privilege
half of RFD §10 ("encrypted credential store, never logged") and the auth
substrate the driver contract (§5) and server bindings (§8) depend on.

Credentials are *not* effects in the plan: resolution happens at `COMMIT` time
when the interpreter binds a driver leg to a live client. Resolution is pure and
side-effect-free up to the point of reading bytes from the store, keeping the
purity invariant (§3) intact — a `Plan` never embeds a secret, only an account
*selector*.

## Scope

In scope:
- `Secrets` trait — the one surface all drivers + server call to fetch a credential.
- Local file backend: encrypted blob, `0600` perms, keyed by `(driver, account)`.
- Cloudflare Workers backend: resolve from Secret Store / `env` bindings (no file).
- Account model + `cfs account` CLI verbs (`add`, `list`, `use`, `remove`).
- Resolution precedence: `--account` > AT `acct` clause > persistent active > sole > error.
- Redaction guarantees: secrets never enter logs, `Debug`, audit ledger, or error text.

Out of scope (deferred):
- OAuth login flows / token refresh logic per driver → **t19 (E4 Google OAuth + multi-account)**.
- `CREATE POLICY` least-privilege enforcement over drivers/verbs → **t35 (E7 server policy / access control)**.
- Audit ledger schema itself → **t12 (E2 audit ledger + observability)**; here we only assert redaction.
- The `AT acct` clause *parsing* lives in the grammar ticket → **t04 (E1 grammar + AST)**; we consume its AST node.

## Key components

New crate `cfs-secrets` (consumer-side small trait, owned DTOs — RFD §9):

```rust
pub struct AccountId(pub String);            // e.g. "work", "personal"
pub struct DriverId(pub String);             // e.g. "mail", "s3"

pub struct CredentialKey { pub driver: DriverId, pub account: AccountId }

/// Opaque secret bytes; redacting Debug/Display, zeroized on drop.
pub struct Secret(/* private */ Zeroizing<Vec<u8>>);
impl fmt::Debug for Secret { /* writes "Secret(<redacted>)" */ }

#[derive(Debug)] // safe: no secret material, only selectors/metadata
pub struct AccountRecord { pub driver: DriverId, pub account: AccountId, pub created_at: OffsetDateTime }

pub trait Secrets: Send + Sync {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError>;
    fn put(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError>;
    fn remove(&self, key: &CredentialKey) -> Result<(), SecretError>;
    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<AccountRecord>, SecretError>;
}

pub enum SecretError { NotFound(CredentialKey), Locked, Backend(String) }
```

- `LocalStore` — impl over `~/.config/cfs/credentials` (XDG), one encrypted blob;
  AEAD (`chacha20poly1305`/`aes-gcm` via `ring`/`rustcrypto`), key from OS keyring or
  passphrase-derived (argon2). Enforces `0600` on create and re-checks on open.
- `WorkerStore` — `wasm32` impl reading Secret Store / `env` bindings; no fs, no `0600`.
- `ActiveAccounts` — persistent `{driver -> account}` map (plaintext metadata, no secrets).
- `Resolver`:

```rust
pub enum AccountSource { Flag, AtClause, Active, Sole }
pub struct Resolution { pub account: AccountId, pub source: AccountSource }

pub fn resolve(
    driver: &DriverId,
    flag: Option<&AccountId>,       // --account
    at_clause: Option<&AccountId>,  // AT 'acct'
    active: &ActiveAccounts,
    available: &[AccountRecord],
) -> Result<Resolution, ResolveError>;  // Ambiguous / NoneConfigured
```

Capability gating (§3): a driver only resolves accounts for its own `DriverId`;
cross-driver key access is impossible by construction (key is `(driver, account)`).

## Implementation steps

1. Scaffold `cfs-secrets` crate; add `zeroize`, an AEAD crate, `argon2`, `time`.
2. Define DTOs (`AccountId`, `DriverId`, `CredentialKey`, `Secret`, `AccountRecord`)
   with redacting `Debug` and `Zeroizing` backing; unit-test redaction.
3. Define the `Secrets` trait + `SecretError`.
4. Implement `LocalStore`: open/create encrypted blob, enforce `0600` (and parent dir),
   AEAD encrypt/decrypt, atomic write (temp + rename).
5. Implement `ActiveAccounts` persistence (separate plaintext metadata file).
6. Implement `resolve()` with the full precedence ladder + structured errors.
7. Feature-gate `WorkerStore` behind `cfg(target_arch = "wasm32")`; share the trait.
8. Wire `cfs account add|list|use|remove` CLI subcommands onto the store/resolver.
9. Expose a `Secrets` handle in the driver-bind context so any driver fetches via the trait.
10. Add golden tests for resolution outcomes and a redaction integration test.

## Considerations

- **Least-privilege & secrets**: `Secret` is the only type holding key material;
  it never derives `Clone`/`Serialize`, redacts `Debug`/`Display`, and zeroizes on
  drop. Lint/grep CI guard: no `format!`/`tracing` call may take a `Secret`.
- **0600 / perms**: on POSIX enforce + re-verify mode on open (reject if group/other
  readable). On Workers there is no fs — `WorkerStore` is the only backend and binds
  to Secret Store, so the `0600` path is compiled out, not skipped at runtime.
- **Idempotency/recovery**: `put` is an atomic temp-write+rename so a crash mid-write
  never corrupts the blob; `account use` is last-writer-wins and replayable.
- **Hard part — key management**: deriving/holding the AEAD key. Resolve by preferring
  the OS keyring (secret-service/Keychain) with an argon2 passphrase fallback; document
  the threat model (at-rest only; a compromised host with the key is out of scope).
- **Hard part — resolution ambiguity**: "sole account" must mean *sole account for
  that driver*, not globally; ambiguity (>1, no flag/clause/active) is a structured
  `ResolveError::Ambiguous` listing candidates — AI-actionable, never a silent pick.
- **Observability**: log resolution *decisions* (driver, chosen account, `AccountSource`)
  but never the credential; this feeds the audit ledger (§6) for "who ran as whom".
- **Directory/coding standards**: keep the trait consumer-side and owned-DTO only;
  no vendor SDK types cross the boundary (§9).

## Acceptance criteria

- `cargo build`, `cargo build --target wasm32-unknown-unknown`, and
  `cargo clippy --all-targets -- -D warnings` are green.
- Unit test: `Secret` `Debug`/`Display` output contains no key material (redaction asserted);
  a grep/clippy guard rejects logging a `Secret`.
- Golden tests over `resolve()` cover every precedence rung and both error variants:
  flag wins over AT clause wins over active wins over sole; zero accounts → `NoneConfigured`;
  multiple, none selected → `Ambiguous{candidates}`.
- `LocalStore` round-trips `put`→`get`→`remove`; created file is mode `0600` and a
  group/world-readable file is rejected on open.
- Atomic-write test: a simulated failure between temp-write and rename leaves the prior
  blob intact and decryptable.
- No live credentials in any test — all stores use fixtures/tempdirs; CI uses no real tokens.
