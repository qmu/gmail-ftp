---
created_at: 2026-06-26T10:02:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort:
commit_hash:
category:
depends_on: [20260626100100-t43-envelope-encryption-sqlite-secret-store.md]
---

# t44 — Rename `accounts` → `connections` (free `accounts` for human identity)

## Overview

The closing slice of **M0 — Persistence foundation**, implementing roadmap decision **B**: "service
credentials are renamed **`connections`** to free `accounts` for human identity." Today the credential
concept is called *account* throughout the secrets crate and CLI; M1 (t45) needs the word `accounts`
for **linked sign-in identities** (roadmap §4.1: identity ≠ authorization). This ticket is a
**behavior-preserving rename** of the credential concept across the secrets crate, the CLI surface,
the binary wiring, and the docs: `qfs account add/list/use/remove` becomes
`qfs connection add/list/use/remove`. Nothing about resolution, encryption, or storage changes — the
SQLite envelope store from t43 is the backend; only the *name* of the concept moves. This is genuinely
mechanical but cross-cutting, and it must land **before** t45 so `accounts` is free.

## Exact seams

- `crates/secrets/src/key.rs` — `AccountId` → `ConnectionId`, `CredentialKey { driver, account }` →
  `{ driver, connection }`, `AccountRecord` → `ConnectionRecord`. `DriverId` is unchanged.
- `crates/secrets/src/active.rs` — `ActiveAccounts` → `ActiveConnections` (the DB-backed table from
  t43; rename the table/columns in a t42-runner migration too).
- `crates/secrets/src/resolve.rs` — `resolve()`, `AccountSource` → `ConnectionSource`, `Resolution`
  field `account` → `connection`. The precedence ladder (`--connection` > `AT` clause > active > sole
  > error) is unchanged; only the names move.
- `crates/secrets/src/store.rs` — `trait Secrets { get/put/remove/list }` signatures take the renamed
  `CredentialKey`/return `ConnectionRecord`; the trait *name* `Secrets` stays (it is the secret
  surface, not the account concept). `crates/secrets/src/backends.rs` `EnvStore` env-var scheme
  `QFS_SECRET_<DRIVER>_<ACCOUNT>` → `QFS_SECRET_<DRIVER>_<CONNECTION>` (document the variable rename;
  consider a read-time alias — see Considerations).
- `crates/cmd/src/lib.rs` — `enum AccountVerb { Add, List, Use, Remove }` → `ConnectionVerb`;
  `enum AccountAction` → `ConnectionAction`; `type AccountLauncher` → `ConnectionLauncher`; the clap
  subcommand `account` → `connection`; `fn account_action(...)` → `connection_action(...)`. The doc
  comments referencing `qfs account <verb>` / "t27, RFD-0001 §10" update to `qfs connection <verb>`.
- `crates/qfs/src/account.rs` → rename file to `crates/qfs/src/connection.rs`; `run_account` →
  `run_connection`, `open_store_for_commit`/`open_store` keep their roles (they open the t43 SQLite
  store). `crates/qfs/src/commit.rs` `networked_credential(driver)` is unaffected in name but its
  `CredentialKey` construction picks up the renamed field. `crates/qfs/src/main.rs` injects the
  renamed launcher into `qfs_cmd::run(...)`.
- `crates/core/src/lib.rs` — re-export of the renamed secrets types.
- Docs to keep honest: `docs/roadmap.md` (the `qfs connection add …` note in §1.1 already anticipates
  this), `crates/skill/assets/SKILL.md` (DESCRIBE→preview→commit examples mentioning `account`),
  README, `docs/guide/*`, `docs/cookbook/*`, and the generated `docs/{language,drivers,server}.md`
  (regenerate via `cargo run -p xtask -- gen-docs` — never hand-edit).

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`).

1. **Rename the secrets-crate types.** `AccountId`/`AccountRecord`/`ActiveAccounts`/`AccountSource`
   and the `CredentialKey.account` field → connection equivalents across
   `crates/secrets/src/{key,active,resolve,store}.rs`; update `crate::lib` re-exports and
   `crates/core/src/lib.rs`. Add the t42-runner migration renaming the t43 secret/active tables &
   columns. Green: secrets-crate unit + resolution golden tests pass under new names.
2. **Rename the CLI surface.** `crates/cmd/src/lib.rs`: `AccountVerb`→`ConnectionVerb`,
   `AccountAction`→`ConnectionAction`, `AccountLauncher`→`ConnectionLauncher`, subcommand
   `account`→`connection`, `account_action`→`connection_action`. Green: `qfs connection --help`
   shows `add/list/use/remove`; clap tests updated.
3. **Rename the binary wiring.** `crates/qfs/src/account.rs` → `connection.rs`,
   `run_account`→`run_connection`; update `crates/qfs/src/main.rs` injection and any `mod account;`
   declarations; `commit.rs` picks up the renamed `CredentialKey` field. Green: `qfs connection
   add/list/use/remove` round-trips against the t43 SQLite store; commit path still resolves.
4. **Env-var scheme + optional deprecation alias.** Rename `EnvStore` `QFS_SECRET_<DRIVER>_<ACCOUNT>`
   semantics → `_<CONNECTION>`. If cheap, keep `qfs account …` as a hidden deprecated alias that
   prints a one-line deprecation notice and forwards to `connection` (FLAG the call — see below).
   Green: env-var resolution test under the new name; alias (if shipped) forwards correctly.
5. **Docs honesty + regenerate.** Update README, `docs/roadmap.md`, `crates/skill/assets/SKILL.md`,
   `docs/guide/*`, `docs/cookbook/*` from `account` to `connection`; run
   `cargo run -p xtask -- gen-docs`. Green: `gen-docs --check` clean.

## Key files

- `crates/secrets/src/{key.rs, active.rs, resolve.rs, store.rs, backends.rs, lib.rs}` (modify).
- `crates/store/src/migrate.rs` / schema (modify): rename t43 tables/columns.
- `crates/cmd/src/lib.rs` (modify): the verb/action/launcher + subcommand rename.
- `crates/qfs/src/account.rs` → `crates/qfs/src/connection.rs` (rename); `crates/qfs/src/main.rs`,
  `crates/qfs/src/commit.rs` (modify).
- `crates/core/src/lib.rs` (modify): re-exports.
- `README.md`, `docs/roadmap.md`, `crates/skill/assets/SKILL.md`, `docs/guide/*`, `docs/cookbook/*`
  (modify); generated `docs/{language,drivers,server}.md` (regenerate, never hand-edit).
- `crates/qfs/Cargo.toml` version bump (next patch).

## Considerations

- **No keyword change.** "connection" is a *path/command* concept, not language vocabulary — this
  rename adds **zero keywords**. The frozen `crates/lang/src/keywords.rs` `KEYWORDS` (38) and
  `OPERATORS` (15) and their freeze tests (`keyword_count_is_frozen`, `operator_count_is_frozen`) are
  untouched. The `AT 'acct'` clause is a grammar construct; if its keyword/identifier surface is
  unaffected the freeze tests stay green — confirm during step 2.
- **Safety floor preserved.** Behavior is unchanged: describe stays pure, preview touches nothing,
  commit stays explicit, irreversible still needs the extra ack. The rename must not alter the
  resolution ladder or the §3 purity invariant (a `Plan` carries a *selector*, never a secret).
- **Redaction unchanged.** `Secret` and its zeroize/redaction are untouched; the rename must not widen
  any `Debug`/log surface. Keep the grep/clippy guard green.
- **Dep-direction discipline.** Pure rename; no new crate, no new runtime edge. The
  `crates/cmd/tests/dep_direction.rs` graph is unaffected (no new leaf).
- **Open product decision to FLAG: deprecation alias.** Whether to keep `qfs account …` as a
  deprecated alias (and for how long) is a UX call. The project is experimental (decision E even
  scraps the vault), so a clean break may be acceptable — flag it for the reviewer rather than
  silently choosing; if shipped, the alias must emit a deprecation notice, not a silent forward.
- **Honesty first.** `docs/roadmap.md` §1.1 already says the command "becomes `qfs connection add …`
  (decision B); the behavior is unchanged" — this ticket is what makes that line true, so the status
  tag for the rename can flip to ✅ only when this PR ships.
- **Versioning.** Own PR + patch bump + `v0.0.x` tag on ship.
