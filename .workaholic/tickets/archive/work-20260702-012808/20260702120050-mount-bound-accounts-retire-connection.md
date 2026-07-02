---
created_at: 2026-07-02T12:00:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX, DB]
effort:
commit_hash: acc0ae3
category: Changed
depends_on: [20260702120010-mount-coordinate-foundation.md, 20260702120040-qfs-app-account-verbs.md]
---

# Mount-bound accounts: the bind path reads the coordinate off the mount; retire `connection`

Part of EPIC `20260702120000` (ADR 0008 §4 — the core behavioral change). Selection state is
abolished: a mount created by `qfs connect /mail gmail you@gmail.com` carries (host='local',
driver, account), and **the bind path resolves the account from the mount**, not from
`active_account` or `QFS_GOOGLE_ACCOUNT`. N accounts of one driver coexist as N paths. The
`connection add/use/list/remove` namespace is retired outright (pre-release hard break).

## CONFIRMED DESIGN + implementation map (owner: option 1, 2026-07-02)

Owner chose **full multi-account** ("2 accounts = N accounts"), design option 1: a **cloud mount
is connect-created**, and the mount's **path leading segment becomes a unique `driver.id()`**
(derived from `mount()` by the `Driver` trait default) backed by a driver of the chosen **kind**
(gmail/gdrive/ga/github/slack/objstore/cf) bound to the mount's **account**:

```
qfs connect /mail  gmail work@x.com   → driver id "mail",  kind gmail, account work
qfs connect /mail2 gmail home@x.com   → driver id "mail2", kind gmail, account home
```

This makes the built-in `/mail`/`/drive`/… mounts **no longer hardwired** — they become
connect-created (the ADR "nothing pre-mounted" model). The owner APPROVED this behavior change.

**Architecture finding (why this is a sub-epic, not one ticket):** all THREE runtime registries
key by `driver.id()` — the `MountRegistry` (describe/plan, `qfs-core`), the `ReadRegistry`
(scan, `qfs-exec`, keyed by the pushdown `SourceId = driver.id()`), and the `DriverRegistry`
(apply, `qfs-runtime`). Path reconstruction (`/{driver.id()}/…`, `plan.rs`), pushdown source ids,
`CALL <id>.proc` qualification, and interpreter grouping ALL derive from `driver.id()`. So a
per-mount account REQUIRES a per-mount `driver.id()`, which requires a per-mount `mount()`.

**The clean implementation (confined to `crate qfs`, NO driver-crate changes):** three thin
**MountAdapter** wrappers — one each for `Driver`, `qfs_exec::ReadDriver`, and
`qfs_runtime::ApplyDriver` — that (a) return the custom `mount()` (so `id()` derives to the
segment), (b) rewrite the mount PREFIX on every inbound path/`ScanNode.source`/`EffectInput.target`
(`/mail2/…` → the inner driver's `/mail/…`) and rewrite embedded paths back on the way out, and
(c) precompute owned rewritten `prelude()` (`SEND → mail2.send`) so the borrowed-return methods
work. `procedures()` is id-agnostic (`ProcSig("send")`, qualified at resolve time) and passes
through; `pushdown()`/`describe`/`capabilities` clone/pass through with the prefix rewrite.

### Sub-steps (each independently green + committable)

1. **MountAdapter module** — the three trait wrappers + a prefix-rewrite helper + unit tests
   (additive, UNWIRED — cannot break anything). The reusable core enabler.
2. **Per-account client build** — a helper that builds a `GoogleStack`/client for ONE account
   email (generalize `live_google_stack`, which today resolves a single active account).
3. **Reshape cloud registration** — `commit.rs` (apply + live read), `shell.rs` (read facets),
   `read_facets.rs`, `describe.rs`: iterate the `path_binding` cloud mounts and register a
   MountAdapter-wrapped, per-account driver/read/apply under each mount's segment id. Retire the
   hardwired built-in `/mail`/`/drive`/`/ga`/… registration.
4. **Account resolution off the mount** — delete `resolve_account_email`'s active-connection read;
   the account is the mount's `path_binding.account`. `QFS_GOOGLE_ACCOUNT` survives ONLY as a CI
   override (documented).
5. **Retire selection + the `connection` namespace** — delete `active_connection`,
   `db_set_active`/`db_get_active`, the `active_account` table (migration v11 drop), and
   `ConnectionVerb`/`ConnectionAction` Add/Use/List/Remove. `qfs account add` stops calling
   `db_set_active` (step 4 makes it unnecessary). Move `rotate`/`revoke` under `qfs account`,
   `rekey` under `qfs vault` (landed 120020). Update read-error strings to the connect flow.
6. **The local Gmail + coexistence smoke** — init → app add → account add (two accounts, stdin
   import) → connect /mail + /mail2 → reads resolve DIFFERENT mailboxes in one process.

This is a focused multi-part push best run deliberately with the owner able to review each green
commit — NOT crammed into a /drive tail while away. The `/drive` for the ADR epic paused here at
5/7 shipped (120010/120020/120030/120040/120060, 1909 tests green, tree clean at the host-skeleton
commit); the design above is locked, so execution can start immediately on the owner's word.

## Steps

1. **Bind resolution from the mount**: `commit.rs::networked_credential` (@591) and the gmail/
   gdrive/ga account resolution (@476/490/504) + `google.rs::resolve_account_email` (@176) take the
   account from the resolved path binding of the leg's mount instead of `active_connection()`.
   `GOOGLE_ACCOUNT_ENV` survives **only** as a CI override (documented as such), checked before the
   mount, never as "selection". `has_consent` (@579) keys on the mount's (driver, account).
2. **Delete selection**: `active_connection()` (`connection.rs:138`), `db_set_active`/
   `db_get_active` + the `active_account` table usage (`secret_store.rs:327-349`, round-trip tests
   @660+), the `ConnectionVerb::Use` arm — all removed. A migration (v11) drops `active_account`
   (append-only ledger; the DROP is a new version).
3. **Retire the verb namespace**: remove `ConnectionVerb`/`ConnectionAction` Add/Use/List/Remove
   (`cmd/src/lib.rs:112,437,926`) and the corresponding arms in `connection.rs::run_inner` (@263).
   `rotate`/`revoke`/`rekey` survive under a coherent home (`qfs account rotate|revoke` for account
   secrets; `qfs vault rekey` landed with `20260702120020`) — decide the exact mapping in-ticket,
   keeping one-concept-one-word. `qfs connection paths` functionality lives on as `qfs connect
   --list` or `/sys/paths` (it lists mounts, which is the connect layer's job).
4. **Read errors**: driver "connect your account" error strings (e.g. gmail's invalid_path message)
   now instruct the `account add` + `connect` sequence.
5. **Local smoke (gate item)**: on this machine — `qfs init` → `qfs app add google <
   ~/.config/gmail-ftp/credentials.json` → `printf %s "$RT" | qfs account add google <email>` →
   `qfs connect /mail gmail <email>` → `qfs run "/mail/inbox |> select date, from, subject |> limit
   3"` returns real rows. No browser.

## Key files

- `packages/qfs/crates/qfs/src/commit.rs` (`networked_credential`, per-driver account resolution,
  `bind_gate` wiring @520-540 — stays fail-closed), `src/google.rs` (`resolve_account_email`,
  GOOGLE_ACCOUNT_ENV @58), `src/connection.rs`, `src/secret_store.rs`, `src/path_binding.rs`
- `packages/qfs/crates/cmd/src/lib.rs` (verb removal + dispatch-test updates)
- `packages/qfs/crates/store/src/lib.rs` (v11 drop of `active_account`)
- `packages/qfs/crates/qfs/src/shell.rs` (@277-299: the shell's cloud read facets resolve accounts
  the same way — verify the shared path)

## Considerations

- **Order matters inside the ticket**: wire mount-resolution first, prove reads/commits green, then
  delete selection — the tree must compile at every commit (one commit is fine too).
- The t54 gate is re-scoped, not weakened: sign-in (operator exists) + consent recorded for the
  mount's (driver, account) — still fail-closed, still before any decrypt.
- `connections.qfs` declarative files: `CREATE CONNECTION` for local SQL/git is untouched (declared
  sources, no secret). Only the credentialed `connection` CLI namespace retires.
- Update `qfs skill` / embedded-skill source text that teaches `connection use`.

## Quality Gate

Global gate (EPIC) plus:

- Hermetic: two mounts of the same driver with different accounts resolve different credentials in
  one process (the coexistence test — the point of the ADR); a mount with no account for a cloud
  driver fails closed with the actionable error; GOOGLE_ACCOUNT_ENV overrides only when set (CI
  path test).
- `active_account` table is gone (v11 assertion); no `db_set_active`/`active_connection` symbol
  remains; `connection` verbs no longer parse — dispatch tests updated.
- **Local smoke passes end-to-end as written in step 5** (owner-decided gate: token import via
  stdin, no browser).
- Repo grep: no non-historical reference to `connection use` remains in crates/ (docs are
  `20260702120070`'s gate).
