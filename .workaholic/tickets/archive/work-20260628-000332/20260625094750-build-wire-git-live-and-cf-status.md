---
created_at: 2026-06-25T09:47:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: -
commit_hash: (superseded/done)
category: Superseded
depends_on: []
---

# git live execution (real RepoStore) + cloudflare (cf) status

## STATUS (2026-06-25, UPDATED): git commit WORKS end-to-end (v0.0.7) — engine bridge built

The engine bridge landed: a `Driver::plan_write(path, verb, args)` seam — `eval_write` asks the
routed driver to supply its own encoded effect plan, defaulting to the generic node. `GitDriver`
overrides it: `INSERT INTO /git/<repo>/commits VALUES ('<msg>', '<branch>')` (positional row — the
describe schema is the read shape, so column names don't apply) lowers via `plan_insert_commit` to
the encoded `tree→commit→ref→reflog` plan. The binary's `git.rs` wires real repos from
`QFS_GIT_<repo>=<path>` (refs snapshot for planning + `RepoStore::at_path` for apply).

**Verified end-to-end** (binary, against a temp repo): `qfs run "INSERT INTO /git/myrepo/commits
VALUES ('add feature via qfs','main')" --commit` writes a real commit — `git log` confirms it on
`main`. Automated regression: `plan_write_seam_lowers_a_values_row_and_commits_via_cli` (driver-git,
the seam + CLI apply) + the git golden corpus (now asserts the 4-node encoded plan) +
`cli_backend_writes_a_real_commit_to_an_on_disk_repo`. 1250 tests green.

Residual (follow-ups, not blockers): the commit identity is a fixed `qfs <qfs@localhost>` + time 0
(no author column / no clock at the pure plan stage — inject a configured identity / add an author
VALUES column later); only `INSERT INTO commits` is lowered (refs UPDATE, `CALL git.merge/checkout/
tag/rebase` still fall through the generic path → ticket); git **reads** (`FROM /git/...`) still need
a real ObjectDb (the read facet). cf remains parked.

---

## (historical) earlier status: real CLI apply backend BUILT + verified; blocked on an ENGINE bridge

The apply half is done: `RepoStore::at_path(<path>)` + `apply_effect_cli` (driver-git) persist a
real commit to an on-disk repo via the `git` CLI — `hash-object -w` for loose objects, the atomic
`update-ref` CAS for the branch, reflog auto-journaled. **Verified** by
`cli_backend_writes_a_real_commit_to_an_on_disk_repo` (driver-git): `plan_insert_commit` + the
CLI-backed applier write a genuine commit, confirmed by the `git` CLI (branch moved to the planned
oid, message + staged file content present). No `gix` dep (ADR-0003-honoured).

**The wall (why `qfs run "INSERT INTO /git/<repo>/commits …"` does NOT yet work):** git is the one
driver whose applier consumes **planner-ENCODED** effects — `effect_from_row` reads an `effect_kind`
discriminator + per-kind columns produced by `plan_insert_commit` (blob→tree→commit→ref→reflog).
But the generic engine (`qfs_core::eval_write`) lowers `INSERT … VALUES` into a **raw** row
(`message`, `branch`), which `decode_node` rejects ("missing `effect_kind` column"). sql/slack/
github work because their appliers read the raw row directly; git needs its driver-specific write
planner to run, and **the engine has no hook to delegate write-planning to a driver**. So the binary
`/git` commit wiring was intentionally NOT shipped (it would fail confusingly mid-commit) — only the
verified driver backend landed.

**Remaining work (the real next step):** add a driver write-planning seam — when `eval_write` targets
a driver that declares custom write lowering (git), call the driver to produce the effect plan
(`plan_insert_commit`/`plan_update_ref`/`plan_merge`) instead of the generic node. Then wire the
binary `/git` driver (the reverted `git.rs` + `QFS_GIT_<repo>` config is drafted in this PR's history)
and the commit works end-to-end. This is an engine/driver-contract change, not more backend code.

## Overview

Two drivers whose live execution was deliberately deferred, grouped because each needs a backend
decision, not just wiring.

### git — needs a real `RepoStore`

`crates/driver-git/` has the real applier/compiler/path/relational logic, but per **ADR-0003**
`gix` was rejected on footprint/offline/wasm grounds and the object reader uses fixture output;
`RepoStore` (the COMMIT apply backing) is not backed by a real repository. `GitDriver::new(repos:
RepoResolver, applier: GitApplier)` → `git_apply_driver` is ready.

- **Build:** a production `RepoStore`/reader over a real git (options: shell out to the `git` CLI —
  zero new deps, the ADR-0003-friendly path; or a `gix`/pack backend — revisit the ADR). Confine to
  a binary-only leaf.
- **Wire:** register under DriverId `git` in `commit.rs` `live_registry()` + a planning mount;
  resolve repo paths (`/git/<repo>@<ref>/...`).
- **Verify:** genuinely E2E-verifiable here against a local temp git repo (offline) — a good first
  slice, unlike the networked drivers. Revisit ADR-0003 explicitly if adding a git dep.

### cf (cloudflare D1 / Workers) — parked, confirm status

The cf worker crate is parked offline (see ADR-0005 / t36 notes; the wasm/worker build is CI-only).
Action: confirm whether live cf execution is in scope at all, or remains parked. If in scope, it
rides the same `HttpExchange` transport pattern (cf has its own seam over `qfs-http-core`) for the
D1/REST surface; the Workers host stays CI-only. Until decided, keep it honestly documented as
parked (do not instruct it in docs/skill).

## Considerations

- ADR-0002/0003/0005 footprint + offline rules govern both backend choices.
- Patch bump + docs-in-lockstep per the umbrella ticket; do not document either as working until a
  live smoke passes.

## DISPOSITION (night drive, 2026-06-29)

git live commit SHIPPED end-to-end v0.0.7 (engine plan_write bridge + RepoStore CLI backend; verified by tests, 1250+ green at the time). The listed residuals (commit identity/author column, refs UPDATE + CALL git.* verbs, git FROM reads via a real ObjectDb) are explicitly non-blocker follow-ups. The cf (Cloudflare worker) status half remains PARKED per CLAUDE.md/ADR-0005 (no cdylib entrypoint; offline cache). Deliverable shipped; no further action this drive.
