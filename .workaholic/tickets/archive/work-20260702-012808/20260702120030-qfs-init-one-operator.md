---
created_at: 2026-07-02T12:00:30+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
commit_hash: ff7f187
category: Added
depends_on: [20260702120020-keyguardian-vault-slots.md]
---

# `qfs init` — the first-run wizard and the one-operator invariant (no password)

Part of EPIC `20260702120000` (ADR 0008 §2). Replace `qfs identity signup` with `qfs init`: one
command that creates the vault (via the KeyGuardian machinery) and registers the operator identity —
**without a password**. Local authentication is the OS login; the email is an accountability label
only. The docs stop calling row-existence "authentication".

## Steps

1. **`qfs init`** (new verb in `packages/qfs/crates/cmd/src/lib.rs` + launcher in
   `crates/qfs/src/main.rs`, logic in a new `crates/qfs/src/init.rs`):
   - Interactive (TTY): prompt for the operator email (echoed, `tty.rs::prompt_line`), then create
     the vault via the guardian flow (passphrase confirm-twice from `resolve_store_passphrase`'s
     first-run branch, or keychain if enrolled) — reusing, not duplicating, the existing prompts.
   - Non-interactive: `qfs init <email>` with `QFS_PASSPHRASE` set — the agent/CI path, no prompt.
   - Idempotent: a second `init` on an initialized host reports what exists (operator + slots) and
     exits 0 — it must be safe to re-run.
2. **One `$HOME` = one operator** (`crates/qfs/src/identity.rs::signup_local` call site @86): a
   second *different* email is refused with an actionable error naming the existing operator and
   pointing at "teams meet on a server host" (self-explanatory-UI policy: what happened + what to
   do). The `SoleUser::Many` arm in `require_signed_in` (`connection.rs:171`) becomes unreachable
   for new stores; keep it as a defensive error for pre-existing multi-user System DBs.
3. **Retire `identity signup`** — remove the verb (`cmd/src/lib.rs` IdentityVerb::Signup @502,
   `identity.rs` Signup arm @71). `identity whoami` stays. `read_password_from_stdin` +
   `validate_password`/argon2id hashing stay in place for the HTTP face (t46) — they are no longer
   reachable from the CLI signup path. Do NOT delete the password machinery.
4. **Gate rewording** (`packages/qfs/crates/secrets/src/consent.rs` ConsentError @49-74 and
   `require_signed_in` messages): errors that say `qfs identity signup <email>` now say `qfs init`.

## Key files

- `packages/qfs/crates/qfs/src/identity.rs`, `src/tty.rs`, `src/connection.rs`
  (`resolve_store_passphrase` first-run branch, `require_signed_in`)
- `packages/qfs/crates/cmd/src/lib.rs` (IdentityVerb, new Init verb + launcher alias, dispatch
  tests @1127+), `crates/qfs/src/main.rs`
- `packages/qfs/crates/secrets/src/consent.rs` (error copy)

## Considerations

- Depends on `20260702120020` so vault creation goes through the slot machinery (init enrolls the
  passphrase slot #0; optionally offers keychain enrollment in the wizard).
- Honest-docs follow-through happens in `20260702120070`; this ticket only changes code + error
  copy.
- The `qfs skill` embedded agent instructions (`qfs-skill` crate) mention setup commands — check
  and update its source so `gen-docs --check` stays green.

## Quality Gate

Global gate (EPIC) plus:

- Dispatch test: `qfs init` routes to the injected launcher (sentinel pattern, cmd tests @1127+).
- Hermetic tests: fresh store + `init <email>` creates operator + slot-0; second `init` same email
  is idempotent (exit 0, no duplicate rows); second `init` different email fails with the
  documented error text; non-interactive path works with `QFS_PASSPHRASE` + argv email and no TTY.
- `identity signup` no longer parses as a verb; `identity whoami` still works.
- Every ConsentError / require_signed_in message names only live verbs (asserted in the existing
  consent tests @126-179).
