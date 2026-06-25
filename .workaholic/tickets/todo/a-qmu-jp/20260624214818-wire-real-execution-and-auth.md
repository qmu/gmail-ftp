---
created_at: 2026-06-24T21:48:18+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash:
category:
depends_on: []
---

# Wire the real execution + auth path into the binary (commit, credentials, account)

## Overview

The qfs binary today is **describe + preview only**. Verified against the binary:
`qfs run --commit` prints `"committed": true` but applies nothing (no file written), and
`qfs account add/list/use/remove` return `not_implemented`. The drivers, the credential store
(`qfs-secrets`), the resolver ladder, and the appliers all exist as **libraries** (built and tested
hermetically in the trip) but were never wired into the running binary's execution path. The docs,
the install welcome, and the agent skill currently overstate this.

Make execution real, in slices, starting with the no-credentials local filesystem so the commit
path is proven end-to-end before credentials are involved.

## Root cause (exact seams)

- `crates/cmd/src/lib.rs` `dispatch_run` builds `qfs_exec::ReadRegistry::new()` (empty) and an
  `Engine` with no real drivers, so `qfs_exec::run_oneshot` → `run_oneshot_inner` → `apply_commit`
  has nothing real to apply (and `FROM /x` reads hit an empty read registry).
- The binary (`crates/qfs/src/main.rs`) injects only a **describe-only** registry
  (`DescribeProvider`) + the shell/serve/skill launchers. There is no injected **run engine /
  apply + read registry** of real drivers. `qfs-cmd` may not depend on the `qfs-driver-*` /
  concrete `qfs-secrets` crates (the `tests/dep_direction.rs` guard) — so, like
  `ShellLauncher`/`DescribeProvider`, the real engine must be **injected from the binary**.
- `crates/cmd/src/lib.rs` `dispatch_account` is the original E0 stub returning `NotImplemented`.
  The store exists: `qfs_secrets::{LocalStore (encrypted vault, argon2id, 0600, atomic),
  EnvStore, ActiveAccounts, Secrets get/put/list/remove, default_credentials_path}`.

## Implementation steps (sliced; each slice its own PR + patch bump)

1. **Slice 1 — local-fs execution end-to-end (no credentials).**
   - Add an injected run-engine/registry provider from the binary (mirror `DescribeProvider`):
     register `qfs-driver-local`'s read facet + `LocalApplier` into the run registries, build the
     `Engine` from it, and have `dispatch_run` use it. Keep tokio confined per the existing
     runtime-leaf rules (the binary is the allowlisted leaf; use the `PlanApplierBridge` as the
     drivers already expect).
   - Result: `qfs run "UPSERT INTO /local/<path> VALUES ('…')" --commit` actually writes the file,
     and `FROM /local/<dir>` actually lists. Verify by writing + reading a real temp file.
   - Update the e2e tests that currently assert commit-is-a-noop.

2. **Slice 2 — credential resolver + env-var auth.**
   - Wire the `qfs_secrets::resolve` ladder into the commit path and inject a `Secrets` store. Start
     with `EnvStore` (`QFS_SECRET_<DRIVER>_<ACCOUNT>`), so a credentialed driver can authenticate
     via env vars with no interactive setup (the agent-friendly path). Surface a clear
     `NoneConfigured`/`Ambiguous` error (the ladder already models these) instead of a silent pass.

3. **Slice 3 — `qfs account` CLI → persistent store.**
   - Implement `add/list/use/remove` against `LocalStore` (injected from the binary, mirroring the
     other launchers). **Decide the auth UX first** (a real product decision — flag it):
     passphrase from `QFS_PASSPHRASE` env or an interactive no-echo prompt; persist a per-store
     salt beside `default_credentials_path()`; read the secret from stdin/prompt, **never argv**.
     `list` prints metadata only; `use` writes `ActiveAccounts`; `remove` is idempotent.
   - Replace the `account_verbs_dispatch_to_structured_not_implemented` test with real behavior.

4. **Slice 4 — remaining drivers' live execution.**
   - Wire each driver's read + apply facet into the run registry behind its real client (mail,
     drive, sql, github, slack, s3/r2, cf, git, ga), one at a time, each with a live-smoke check.

5. **Docs honesty (do alongside slice 1, not after).**
   - Until a capability is real, the install welcome, README, cookbook, getting-started, accounts
     page, and the agent skill must not instruct it. Update them slice-by-slice to match what works.
   - Also fix the example domain: `a@b.com` → `alice@example.com` (RFC 2606) in install.sh,
     `plugins/qfs/skills/qfs/SKILL.md`, and `docs/guide/cli.md`.

## Key files

- `crates/cmd/src/lib.rs` (`dispatch_run`, `dispatch_account`, the injected-provider types, the
  `pub fn run` signature), `crates/qfs/src/main.rs` (composition root), `crates/qfs/src/describe.rs`
  (the existing provider to mirror).
- `crates/driver-local/src/applier.rs` (`LocalApplier`), `crates/exec/src/lib.rs`
  (`run_oneshot_inner`, `apply_commit`, `ExecCtx`, `ReadRegistry`).
- `crates/secrets/src/{local.rs,backends.rs,resolve.rs,active.rs}` (store + resolver), and the
  `tests/dep_direction.rs` allowlist (any new binary→driver/secrets edges land on the binary only).
- Tests: `crates/cmd/tests/e2e_cli.rs`, `crates/cmd/src/lib.rs` unit tests asserting NotImplemented.

## Considerations

- **Honesty first:** never document a capability before it works (this whole ticket exists because
  that rule was broken). Keep docs in lockstep with each slice.
- **Security (運用/設計):** secrets never on argv, never logged, never in errors (`Secret` already
  redacts). The `LocalStore` is encrypted + `0600`; the passphrase source is a deliberate UX choice
  to confirm, not to guess.
- **Dep direction:** `qfs-cmd` stays off the concrete driver/secrets crates; all real wiring is
  injected from the terminal binary (the allowlisted leaf), as the existing guards require.
- **Tokio confinement:** the apply path uses the runtime bridge the drivers already provide; keep
  tokio dead-ending in the binary leaf.
- **Scope per PR:** ship one slice at a time (each a PR + patch bump), each leaving the tree green
  and the docs honest about exactly what now works.
