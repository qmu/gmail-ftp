---
created_at: 2026-06-30T20:32:00+09:00
author: a@qmu.jp
type: bugfix
layer: [Infrastructure]
effort: 2h
commit_hash: 05cedc4
category: Changed
depends_on: []
---

# This host's `~/.config/qfs/project.db` won't open on this branch (migration v2 mismatch)

## Symptom (found 2026-06-30)

Any store-touching command (e.g. `qfs connection add ...`) on this host fails:

```
opening the project database: migration v2 was edited in place after being applied
(recorded 1be5979f…, embedded 97466be6…); ship a NEW version instead
```

So the pre-existing `~/.config/qfs/project.db` (+ `system.db`), created by an installed/older qfs,
cannot be opened by this branch's code. (Verified the rest of this cycle against a throwaway
`XDG_CONFIG_HOME` to avoid touching the owner's real DB — do NOT delete it.)

## Investigate / fix

1. Determine whether migration **v2 was actually edited in place** on this branch (a real bug — the
   migration runner hashes embedded vs recorded) or whether the host DB is from a **different qfs
   version** (expected skew). Check `git log -p` for the v2 migration SQL + the recorded vs embedded
   hashes.
2. If a real in-place edit: ship the change as a **new migration version** (the error's own advice),
   never edit an applied migration.
3. Provide a safe path for an existing DB: a documented `qfs` migration/repair, or a clear
   "incompatible DB — back up and re-init" message + a `--reset`/re-init flow (the owner's real
   connections/identities live here, so a silent wipe is unacceptable).

## Key files

- `crates/qfs/src/store.rs` (`open_project_db`, the migration runner + hash check), the migration SQL
  set, `crates/store` (if the migration framework lives there).

## Considerations

- Blocks live verification of the gmail-ftp/gdrive-ftp replacement on the real host DB (EPIC
  `20260630203000` / ticket `20260630203030`). High priority for the owner's daily use.

## Final Report

Development completed — with a **deliberate deviation from the ticket's prescribed fix**.

The ticket offered two branches: (1) if a real in-place edit, ship the change as a NEW migration
version; (2) otherwise (version skew), a documented repair / reset. Investigation found it IS a real
in-place edit — commit `f95d20c` renamed the credential column `account`→`connection` by editing
Project migration v2's body (`1be5979f…`→`97466be6…`) — but the ticket's premise that this hadn't
shipped was **false**: `f95d20c` is tagged **v0.0.9** (the installed host binary). So two DB lineages
exist in the wild: pre-v0.0.9 (`account` column, the owner's host) and v0.0.9+ (`connection` column,
every throwaway DB this cycle). Reverting v2 (branch 1) would have fail-closed every v0.0.9 DB.

Implemented instead a **superseded-checksum forward-heal** (a third, correct branch for *a botched
in-place edit that already shipped*): the migration runner carries a `SUPERSEDED_BODIES` registry
keyed by the OLD body's checksum; when it meets a recorded checksum that is a registered superseded
body, it heals that DB forward (renames the columns) and re-stamps the recorded checksum in one
transaction, instead of erroring. An UNLISTED mismatch still fails closed, so tamper-evidence holds
for genuinely-unknown edits. No data wipe, no manual step — the owner's connections survive.

### Discovered Insights

- **Insight**: An in-place migration edit that ships in a release is unrecoverable by the "add a new
  version" rule the runner's own error advises — that rule assumes the bad body never reached a real
  DB. Once released, two lineages coexist and no single embedded checksum satisfies both; the only
  non-destructive fix is a forward-heal keyed off the old body's checksum.
  **Context**: `f95d20c`'s commit message explicitly justified the in-place edit as "safe because…
  no released DB exists yet" — an assumption that silently expired the moment v0.0.9 shipped. The new
  `SUPERSEDED_BODIES` mechanism is the general escape hatch for any future repeat.
- **Insight**: Checksums are content-addressed, so a global `(old_checksum → reconcile SQL)` registry
  needs no scope/version key — only a DB that applied EXACTLY that body records that checksum, so the
  reconcile SQL is guaranteed to run against the schema that body created.
  **Context**: System v2 and Project v2 share a version number but never a checksum; keying the heal
  by checksum alone is unambiguous across scopes.
