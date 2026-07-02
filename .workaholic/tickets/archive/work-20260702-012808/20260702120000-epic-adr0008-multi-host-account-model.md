---
created_at: 2026-07-02T12:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain, DB]
effort:
commit_hash: 15e6bae
category: Changed
depends_on: []
---

# EPIC: ADR 0008 — the multi-host account model

## The decision (owner, 2026-07-02)

[ADR 0008](../../../../docs/adr/0008-multi-host-account-model.md) (commit `5f2991f`): qfs must work
as a business — open source + self-host + a managed service arguing its value on the same code. The
decided shape: **the CLI is a multi-host client, local is an implicit host** (gh-CLI style), the
account surface splits into per-layer verbs, selection state is abolished (the mount carries the
full coordinate), and the vault key generalizes to KeyGuardian slots with managed KMS as one slot.

This epic implements the "reserved now" half of the ADR. Multi-tenant internals, billing, the
remote-host protocol, and cross-host execution stay deferred per ADR §6.

## What changes (the five defects it fixes)

1. Unverified CLI "sign-in" → `qfs init` with **no password** (OS-delegated auth, honest docs).
2. The second-signup cliff → **one `$HOME` = one operator** invariant, refused with a clear error.
3. The `connection` grab-bag → per-layer verbs: `init` / `host` / `app` / `account` / `connect`.
4. Two selection mechanisms → **the mount carries (host, driver, account)**; `active_account` dies.
5. Passphrase-hardwired vault → **KeyGuardian slots** (passphrase + OS keychain now; agent/KMS later).

## Sub-tickets (dependency order)

1. `20260702120010` — Mount-coordinate foundation: schema v9 (`host`/`account` on `path_binding`),
   grammar `ACCOUNT`/`HOST` clauses, row/IO threading. *(no deps)*
2. `20260702120020` — KeyGuardian vault-key slots: v10 `vault_key_slot` table, N-slot envelope,
   passphrase + OS-keychain slots, `qfs vault` verb. *(no deps)*
3. `20260702120030` — `qfs init` + the one-operator invariant (subsumes `identity signup`).
   *(depends: 120020)*
4. `20260702120040` — `qfs app` + `qfs account` verbs (dissolve google-app / google-token / consent
   out of `connection`). *(depends: 120030)*
5. `20260702120050` — Mount-bound accounts: `connect` binds the coordinate, bind path reads the
   account off the mount, retire `connection add/use/list/remove` + `active_account`.
   *(depends: 120010, 120040)*
6. `20260702120060` — `qfs host` skeleton: gh-style hosts records, login records without a network
   protocol. *(depends: 120030)*
7. `20260702120070` — The docs hard-break sweep + patch bump: every cookbook/guide/generated
   doc/skill on the new verbs; repo-wide retired-verb grep reaches zero. *(depends: 120050, 120060)*

## Global Quality Gate (owner-interrogated, 2026-07-02)

Every sub-ticket must pass before its `/drive` approval:

- `cargo test --workspace` green (hermetic; includes the cookbook parse ratchet).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean.
- `cargo run -p xtask -- gen-docs --check` and `gen-skills --check` in sync.

Additionally, per owner decision:

- **Retired-verb zero (final gate, owned by 120070):** a repo-wide grep for `connection add`,
  `connection use`, `connection list`, `connection remove`, `identity signup` finds zero references
  outside the migration ledger, ADR/archive history, and CHANGELOG-type records.
- **Local smoke (owned by 120050):** on this machine, the sequence `qfs init` → `qfs app add google
  < credentials.json` → `qfs account add google` (refresh-token import via stdin — NO browser) →
  `qfs connect /mail gmail <email>` → a real `/mail/inbox` read succeeds end-to-end. Browser-consent
  E2E is explicitly OUT of the gate (interactive seam).
- **KeyGuardian (owned by 120020):** hermetic slot enroll / multi-slot unlock / rekey tests; the
  keychain slot degrades cleanly (clear error, no panic) on a host without a secret service.
- **hosts skeleton (owned by 120060):** hosts-record round-trip tests; `host login` performs no
  network I/O.

## Standing constraints

- Pre-release **hard break**: no compat shims, no deprecation period (repo rule). Retire, don't alias.
- Commits via `commit.sh` only; bump the patch version on the shipped PR; tag `v0.0.x` on ship.
- Generated docs / SKILL.md are never hand-edited — change source, regenerate.
- Append-only checksummed migrations: new versions only, never edit shipped schema bodies.
- Secrets never on argv; TTY prompts echo-off; selector tables stay passphrase-free.
- New grammar words (`ACCOUNT`, `HOST`) are contextual idents, never frozen keywords (t31 lesson).

## Related history

- `archive/work-20260629-110121/20260630004100` (in-language connection declaration) and
  `…004170` (connection CLI alignment) — built the surface this epic dissolves.
- `archive/work-20260628-000332/20260626100100` (t43 envelope vault) — the mechanism KeyGuardian
  generalizes; t80 `e2e_store.rs` is the existing N-wrap precedent.
- Current branch `work-20260702-012808` — the passphrase/operator/account-model docs and ADR 0008.
