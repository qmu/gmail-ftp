---
created_at: 2026-07-02T13:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, DB, UX]
effort:
commit_hash: 7c756ab
category: Changed
depends_on: []
---

# RESUME: ADR 0008 account model — 5/7 shipped, continue at 120050 (mount-bound accounts)

Context-window checkpoint for a fresh `/drive`. The ADR 0008 multi-host account-model epic
(`20260702120000`) is **5 of 7 sub-tickets shipped and green**; the two remaining are the core
behavioral change (`120050`) and the docs sweep (`120070`, depends on 120050). The owner has
CONFIRMED the 120050 design (full multi-account, "2 = N"); the full implementation map is already
written into the `120050` ticket body. **Do 120050 next, then 120070.**

## Position (verified 2026-07-02)

- Branch `work-20260702-012808`, HEAD **`fa8c6f2`** ("Add the qfs host skeleton with no protocol").
  10+ commits ahead of `main`, **unpushed, no PR yet**.
- **Whole workspace green: `cargo test --workspace` = 1909 passed, 0 failed** (last run after
  120060). clippy `-D warnings`, `fmt --check`, `gen-docs --check`, `gen-skills --check` all clean.
- Working tree is clean EXCEPT one intentional, uncommitted edit:
  `.workaholic/tickets/todo/a-qmu-jp/20260702120050-…md` carries the confirmed design + the 6-step
  implementation map (see below — reproduced here so this resume ticket is self-contained). Commit
  that ticket note with the first 120050 commit, or discard and rely on this resume ticket.
- Shipped ADR 0008 sub-tickets (archived under `archive/work-20260702-012808/`): `120010`
  mount-coordinate columns (schema v9) `a620abf`; `120020` KeyGuardian vault slots (v10, OS
  keychain via pure-Rust zbus) `2cc965f`; `120030` `qfs init` + one-operator invariant (retired
  `identity signup`) `0d5fdb9`; `120040` `qfs app` / `qfs account` verbs `fa5548a`; `120060`
  `qfs host` skeleton (v13 hosts table, no protocol) `fa8c6f2`.
- The `connection add/use/list/remove` namespace is deliberately **still live** — the CLI works as
  documented; retiring it is 120050's job, and the docs sweep (120070) intentionally waits on it.

## The confirmed design for 120050 (owner: option 1, N accounts, 2026-07-02)

A **cloud mount is connect-created**, and the mount's **path leading segment becomes a unique
`driver.id()`** (the `Driver` trait derives `id()` from `mount()` by stripping the leading `/`),
backed by a driver of the chosen **kind** (gmail/gdrive/ga/github/slack/objstore/cf) bound to the
mount's **account**:

```
qfs connect /mail  gmail work@x.com   → driver id "mail",  kind gmail, account work
qfs connect /mail2 gmail home@x.com   → driver id "mail2", kind gmail, account home
```

The built-in `/mail`/`/drive`/… mounts become **connect-created** (the ADR "nothing pre-mounted"
model). The owner APPROVED this behavior change (`account add` then a `qfs connect /mail gmail
<email>` step replaces "`/mail` just works after account add").

**Why it is a sub-epic, not one ticket:** all THREE runtime registries key by `driver.id()` — the
`MountRegistry` (describe/plan, `qfs-core`), the `ReadRegistry` (scan, `qfs-exec`, keyed by the
pushdown `SourceId = driver.id()`), and the `DriverRegistry` (apply, `qfs-runtime`). Path
reconstruction (`/{driver.id()}/…` in `plan.rs`), pushdown source ids, `CALL <id>.proc`
qualification, and interpreter grouping ALL derive from `driver.id()`. So a per-mount account
REQUIRES a per-mount `driver.id()`, which requires a per-mount `mount()`.

**Clean implementation (confined to `crate qfs`, NO driver-crate changes):** three thin
**MountAdapter** wrappers — one each for `qfs_driver::Driver`, `qfs_exec::ReadDriver`,
`qfs_runtime::ApplyDriver` — that (a) return the custom `mount()` so `id()` derives to the segment,
(b) rewrite the mount PREFIX on every inbound path / `ScanNode.source` / `EffectInput.target`
(`/mail2/…` → the inner driver's `/mail/…`) and rewrite embedded paths back on output, and (c)
precompute owned rewritten `prelude()` (`SEND → mail2.send`). `procedures()` is id-agnostic
(`ProcSig("send")`, qualified at resolve time by `resolve.rs`) and passes through; `pushdown()`
clones through.

## Remaining work — do in order

### 120050 (sub-steps, each independently green + committable)

1. **MountAdapter module** in `crate qfs` — the three trait wrappers + a prefix-rewrite helper +
   unit tests (additive, UNWIRED — cannot break anything). The reusable enabler.
2. **Per-account client build** — generalize `crate::google::live_google_stack`
   (`crates/qfs/src/google.rs`, currently resolves ONE active account via `resolve_account_email`)
   into a builder for a `GoogleStack` bound to a GIVEN account email.
3. **Reshape cloud registration** — iterate the `path_binding` cloud mounts and register a
   MountAdapter-wrapped, per-account driver/read/apply under each mount's segment id, retiring the
   hardwired built-ins in: `crates/qfs/src/commit.rs` (`register_google` @~467, `networked_credential`
   @591, live read), `crates/qfs/src/shell.rs` (read facets @~270-320),
   `crates/qfs/src/read_facets.rs`, `crates/qfs/src/describe.rs` (`register_defined_paths` @~135).
4. **Account off the mount** — delete the active-connection read in
   `crates/qfs/src/google.rs::resolve_account_email` (@156); the account is the mount's
   `path_binding.account`. `QFS_GOOGLE_ACCOUNT` survives ONLY as a CI override (document it).
5. **Retire selection + `connection`** — delete `active_connection` (`connection.rs` @205),
   `db_set_active`/`db_get_active` (`secret_store.rs` @473/@486), the `active_account` table
   (**new migration v11 DROP**; append-only ledger, never edit shipped bodies), and
   `ConnectionVerb`/`ConnectionAction` Add/Use/List/Remove (`cmd/src/lib.rs`, `connection.rs`
   `run_inner`). `qfs account add` stops calling `db_set_active` (step 4 makes it unnecessary — see
   `account.rs::add_google`/`add_cloud`). Move `rotate`/`revoke` under `qfs account`, `rekey` under
   `qfs vault` (landed 120020). Update the audit `connection` field in `commit.rs::audit_events`
   (@148, currently `active_connection`) to the mount's account. Reword read-error strings.
6. **Smoke** — init → app add → account add (TWO accounts via stdin import) → `connect /mail` +
   `connect /mail2` → the two mounts resolve DIFFERENT mailboxes in one process (the coexistence
   proof). Use `~/.config/gmail-ftp/credentials.json` for the app; token import is stdin (no
   browser).

### 120070 (docs hard-break sweep) — after 120050

Rewrite every cookbook/guide/generated-doc/skill to the new verbs (init/host/app/account/connect,
the connect-created-mount flow), bump the patch in `crates/qfs/crates/qfs/Cargo.toml`, and drive
the **retired-verb-zero** grep gate. Full scope is in that ticket.

## Quality gate (every sub-ticket, from the EPIC)

`cargo test --workspace` green (hermetic, incl. the cookbook parse ratchet) · `cargo clippy
--workspace --all-targets -- -D warnings` (NOT `--all-features`) · `cargo fmt --all --check` ·
`cargo run -p xtask -- gen-docs --check` · `gen-skills --check`. Plus 120050's local coexistence
smoke and 120070's retired-verb-zero grep.

## Build-host operational notes (this EC2 box — DO NOT relearn the hard way)

- `cd packages/qfs` for all cargo. **Export `TMPDIR=/home/ec2-user/projects/qfs/.tmp` and
  `CARGO_INCREMENTAL=0`** for every cargo run — `/tmp` is a small tmpfs, and incremental artifacts
  filled the 100G disk mid-suite (deleting `target/debug/incremental` freed ~9.5G; `cargo clean`
  frees ~10G more when `/home` fills).
- `rm` is trash-aliased — use `command rm` to actually free space. zsh `noclobber` is on — use
  `>|` to overwrite a redirect target.
- Full `cargo test --workspace` takes ~8-10 min — run it in the BACKGROUND and await the
  completion notification; don't foreground-block.
- Commit ONLY via `commit.sh` (workaholic). Archive a finished sub-ticket via
  `skills/drive/scripts/archive.sh <ticket> "<title>" <repo-url> <why> <changes> <concerns>
  <insights> <verify>`. The `/drive` owner-approval gates timed out to auto-proceed this session
  (owner was AFK); confirm the owner's preferred cadence on resume.
