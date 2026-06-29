---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: L
commit_hash: 7ca2321
category: Added
depends_on: [20260626100000-t42-persistence-sqlite-system-project-db.md, 20260626100300-t45-identity-users-accounts-local-signup.md]
---

# t53 — `/sys/*` driver + first admin views

## Overview
Delivers the principle the roadmap names for administration (§3.4, §4.2, M3): **administration is
also "everything is a path."** A new `qfs-driver-sys` exposes the deployment's own state —
`/sys/users`, `/sys/policies`, `/sys/connections`, `/sys/audit`, `/sys/projects` — as ordinary qfs
paths backed by the System DB (t42), and the dashboard renders its first admin views over those
paths. Crucially this preserves one-engine-three-faces: a super-admin can do every administrative
action as a qfs statement (`FROM /sys/audit |> WHERE …`, `INSERT INTO /sys/policies VALUES (…)`),
the CLI and MCP face reach the same paths, and the admin page is just the dashboard rendering that
same engine. What already exists as a *pattern* (not as `/sys` code): `crates/server/src/driver.rs`
`ServerDriver` is the exact template — a driver whose introspective half is pure and whose real
mutation is a runtime applier (the `NoopApplier`/`ServerConfigApplier` split). What is genuinely
new: the `qfs-driver-sys` crate, its System-DB-backed applier, the `/sys/*` node schemas, and the
admin views. The System DB tables themselves are owned by t42 (skeletons) and t45 (`users`/
`accounts`); this ticket reads/writes them through a path façade, it does not redefine them.

## Exact seams
- `crates/server/src/driver.rs` — `ServerDriver` (impl `Driver`), `SERVER_MOUNT = "/server"` (line
  28), pure introspection + `NoopApplier` (line 143), real mutation via runtime
  `ServerConfigApplier`. THE pattern to mirror: `qfs-driver-sys` is `SysDriver` with
  `SYS_MOUNT = "/sys"`, a pure `describe()`/`capabilities()`, and a `NoopApplier` for the pure
  half — live mutation lands in a runtime `SysConfigApplier` injected from the binary.
- `crates/driver/src/lib.rs` — `pub trait Driver` (mount/id/describe/capabilities/procedures/
  pushdown/prelude/plan_write/applier), `Archetype` (RelationalTable for `/sys/*` tables;
  AppendLog for `/sys/audit`), `Capabilities`, `Verb`, `NodeDesc`. The introspective half stays
  pure (proof test `fixture_driver_introspection_is_pure`); only `applier()` is impure.
- `crates/core/src/ddl/server.rs` `server_node_schema(node)` — the single-source-of-truth pattern
  for `/server/*` schemas; mirror it with a `sys_node_schema(node)` so `DESCRIBE /sys/users` returns
  a stable, cred-free schema. `src/ddl/server/spec.rs` `StatementSpec`/`PlanSpec` shows the
  serializable-AST discipline to follow if a `/sys` write must store a body.
- `crates/store`/`qfs-persist` (NEW from t42) — the System DB connection-opening seam. `SysDriver`'s
  applier reads/writes the System DB through t42's connection seam (designed so a reverse proxy can
  inject the tenant→DB route later, decision F); it never opens a path itself.
- `crates/qfs/src/describe.rs` `describe_registry()` (cred-free) and `crates/qfs/src/commit.rs`
  `live_registry()` — register `SysDriver`'s pure half in the describe registry and its live applier
  in the commit registry, exactly as the other drivers are. Composition root: `crates/qfs/src/main.rs`
  → `qfs_cmd::run(...)`.
- `crates/secrets/src/store.rs` `Secrets` trait — `/sys/connections` projects connection
  *names/metadata/scopes only*, never secret material (the redaction contract the roadmap §3.2
  states: "names + metadata only, never secrets"). It reads the connection registry, not the vault.
- `crates/server/src/policy/` — `model.rs` (`Policy`,`Rule`,`Verb`), `enforce.rs`
  `evaluate(policy, plan) -> PolicyDecision` (default-deny, pure), `gate.rs` `gate_plan`. `/sys/policies`
  reads/writes policy rows; the extended ACL language is t57 — here `/sys/policies` is the path
  façade over the existing policy model, not new grammar.
- Dashboard: t51 shell + t52 cards render the first admin views by issuing `FROM /sys/...` reads and
  (gated) `INSERT INTO /sys/...` writes through the SAME `/api/preview`+`/api/commit` bridge.
- `crates/cmd/tests/dep_direction.rs` — add `qfs-driver-sys` to the `qfs-driver-*` runtime-consumer
  allowlist; the driver crate must not pull tokio (its applier is sync over rusqlite, wired from the
  binary leaf).
- Template: `crates/driver-local/` (`LocalFsDriver`/`LocalApplier`/`scan_rows`) is the apply-bridge
  reference; `local_apply_driver()` + `qfs_runtime::PlanApplierBridge` is the wiring shape to copy.

## Implementation steps
1. **`qfs-driver-sys` pure half (tree green).** New crate `crates/driver-sys/` with `SysDriver` impl
   `Driver`: `SYS_MOUNT = "/sys"`, `sys_node_schema(node)` for `users/policies/connections/audit/
   projects`, `capabilities()` advertising `SELECT` everywhere + `INSERT/UPSERT/UPDATE/REMOVE` only
   where the System DB supports it (e.g. `/sys/audit` is append/read-only), `plan_write()` producing
   pure effect nodes. Register in `describe_registry()`. `DESCRIBE /sys/users` works with no DB,
   no creds — proof test for introspection purity. `cargo build/test/clippy/fmt` + `gen-docs --check`
   green.
2. **System-DB-backed read path.** Implement the read side over t42's System DB connection seam:
   `FROM /sys/users`, `/sys/projects`, `/sys/audit` scan rows from their tables;
   `/sys/connections` projects names/metadata/scopes from the connection registry (NO secrets);
   `/sys/policies` reads policy rows. Register the live read registry in `commit.rs`/`live_registry()`.
3. **Mutation applier.** Add `SysConfigApplier` (runtime, sync over rusqlite, mirror
   `ServerConfigApplier` + `PlanApplierBridge`) wired from `crates/qfs`. `INSERT INTO /sys/policies`,
   user/connection admin writes apply transactionally to the System DB; `/sys/audit` is append-only
   (no UPDATE/REMOVE). Every `/sys` mutation writes an audit row (the audit log is a path observing
   itself).
4. **First admin views.** In the dashboard (t51/t52), add read views over `/sys/users`,
   `/sys/connections`, `/sys/audit`, `/sys/policies`, `/sys/projects` and a gated policy-grant write
   that flows through the t52 preview→commit card. Keep them thin renderings of the path reads —
   no admin capability that the CLI lacks.
5. **Docs honesty + version.** Document `/sys/*` in the generated drivers reference via
   `cargo run -p xtask -- gen-docs` (NEVER hand-edit `docs/drivers.md`); flip the roadmap §3.4 admin-
   views status tag only after a real `/sys` read+gated-write works. Patch bump in
   `crates/qfs/Cargo.toml`.

## Key files
- NEW `crates/driver-sys/{lib.rs (SysDriver), schema.rs (sys_node_schema), read.rs, applier.rs (SysConfigApplier)}`.
- `crates/qfs/src/describe.rs` — register `SysDriver` pure half; `crates/qfs/src/commit.rs` — register live.
- `crates/qfs/src/main.rs` — inject the `/sys` applier in the composition root.
- `crates/cmd/tests/dep_direction.rs` — add `qfs-driver-sys` to the runtime-consumer allowlist.
- `crates/qfs/assets/dashboard/*` — first admin views (t51/t52 shell).
- `crates/qfs/Cargo.toml` — patch bump; `docs/roadmap.md` + generated `docs/drivers.md` (via gen-docs).

## Considerations
- **Safety floor + redaction.** `/sys/connections` MUST surface names/metadata/scopes only — never
  secret material (roadmap §3.2; the `Secrets` trait stays the only secret reader). `/sys/audit` is
  append-only and every `/sys` mutation appends to it, so administration is itself reviewable as a
  query. Describe stays pure (proof test); preview over `/sys/*` touches nothing; `/sys` writes are
  explicit commits gated by policy and rendered through the t52 card.
- **One-engine constraint is the design test.** The admin page must add NO capability beyond
  `FROM /sys/...` / `INSERT INTO /sys/...`; if a view needs something the path cannot express, the
  fix is to extend the `/sys` path schema, not to add a side-channel admin API. A super-admin doing
  the same action from the CLI must get the same result.
- **Authorization gating.** `/sys/*` writes are high-privilege; they must be policy-gated
  (`crates/server/src/policy` `evaluate`, default-deny) and, once t46/t50 are on, restricted to an
  admin session/role. Until the super-admin vs. project-admin split is settled (below), default to
  loopback super-admin only and flag the gap rather than shipping an open admin surface.
- **Dep-direction.** `qfs-driver-sys` is a `qfs-driver-*` crate: pure introspection in the crate,
  its tokio-free applier wired from the `qfs` binary leaf; add it to the `dep_direction.rs`
  allowlist. The driver must not depend on lang/parser/plan beyond the shared driver/types contract.
- **System DB ownership.** Tables are defined by t42 (skeletons) and t45 (`users`/`accounts`); this
  ticket is a path façade over them. If a column is missing for an admin view, extend it via t42's
  migration runner (do not create a parallel schema).
- **Open product decision to flag (per roadmap §3.4, explicitly NOT to bake in):** which views ship
  first, how much is generated from the `/sys` schema vs. hand-built, and the local-super-admin vs.
  project-admin split. Ship the smallest honest slice (read views + one gated policy-grant write),
  generate from `sys_node_schema` where cheap, and record the split decision in the PR rather than
  hard-coding a privilege model now.
- **Versioning.** One PR, patch bump in `crates/qfs/Cargo.toml`, `v0.0.x` tag on ship; regenerate
  `docs/drivers.md` so `gen-docs --check` stays green.
