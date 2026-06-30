---
created_at: 2026-06-30T20:32:00+09:00
author: a@qmu.jp
type: bugfix
layer: [Infrastructure]
effort: 2h
commit_hash:
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
