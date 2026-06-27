---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: []
---

# t42 — SQLite System DB + Project DB + embedded migrations runner

## Overview

This is the foundation stone of **M0 — Persistence foundation**, implementing roadmap
decisions **E** ("all SQLite, no migration of today's file vault — scrap & build") and **F**
(stateless-at-scale: a trusted reverse proxy injects the tenant→DB route; clients never name a DB).
It delivers the **two databases** roadmap §4.2 names: a **System DB** (per host: projects,
cross-project config, the `/sys/*` surface) and a **Project DB** (per project: that project's
connections, config, state), both on `rusqlite` — which is **already vendored** as a real dep of
`crates/qfs` and a dev-dep of `crates/driver-sql`. What is genuinely **new** is the persistence
crate itself, a **versioned embedded-migrations runner** (grep finds only RFD comments today — no
migrations system exists), and a connection-opening seam shaped so the tenant→DB route can be
injected later (decision F) without the binary ever hard-naming a DB file. This ticket ships only
the runner plus **empty schema skeletons**; the tables are filled by later M0–M3 tickets (t43
secrets, t45 identity, t53 `/sys/*`).

## Exact seams

- **New crate `qfs-store`** (the persistence leaf). It owns `rusqlite::Connection` and is **sync** —
  `rusqlite` is sync, so tokio must NOT enter here (the same confinement that keeps
  `crates/cron`/`crates/server/src/policy` pure). It mirrors how `crates/driver-sql` already uses
  `rusqlite = { version = "0.40", features = ["bundled"] }` as its in-process backend.
- `crates/qfs/Cargo.toml` — already declares `rusqlite = { version = "0.40", features = ["bundled"] }`
  with the comment that the libsqlite3 C build "dead-ends" in the binary leaf; `qfs-store` reuses the
  same workspace dep so there is one libsqlite3 build.
- `crates/cmd/tests/dep_direction.rs` — `TERMINAL_LEAVES = ["qfs", "qfs-skill", "xtask"]` and the
  runtime-consumer allowlist. `qfs-store` is a **new leaf crate** and MUST be added to the
  `cargo metadata` graph allowlists this test checks; the binary `crates/qfs` (the terminal sink
  where things dead-end) is the only crate allowed to open a real DB path.
- `crates/qfs/src/account.rs` — `open_store()` / `open_store_for_commit()` today resolve a path via
  `default_credentials_path()` and unlock a file vault; the **connection-opening seam** added here is
  the analogue the binary will call to open the System/Project DB (t43 then routes secrets through it).
- `crates/qfs/src/main.rs` → `qfs_cmd::run(...)` composition root — the place the binary will call
  the migration runner **on start/relaunch** (roadmap §4.2: "embedded migrations apply System-DB
  changes safely in the same motion" when the binary is updated and relaunched).
- `crates/core/src/lib.rs` — re-exports `qfs-secrets`; once t43 lands, the persistence handle is the
  thing the secrets backend is built over, so keep `qfs-store` consumable from the same hub path.

## Implementation steps

Each slice leaves the tree green (`cargo build/test/clippy/fmt` + `cargo run -p xtask -- gen-docs --check`).

1. **Scaffold `qfs-store`** as a new workspace leaf crate with the shared
   `rusqlite = { version = "0.40", features = ["bundled"] }` dep. Add it to the
   `crates/cmd/tests/dep_direction.rs` allowlists (it is a leaf; nothing in the spine may depend on
   it — only the binary `crates/qfs` opens real DB paths). No tokio. Green: crate builds, dep guard
   passes.
2. **Connection-opening seam.** Define a `Db` handle wrapping `rusqlite::Connection` and a
   `DbSource` trait (or fn-injection, mirroring the launcher pattern in `account.rs`) that **yields a
   connection without the caller naming a file** — the binary supplies the path/route, so a future
   reverse-proxy tenant→DB injection (decision F) is a different `DbSource` impl, not a code change.
   Ship `SystemDb` and `ProjectDb` newtypes over `Db` so the two scopes (roadmap §4.2) are distinct
   types, not a string.
3. **Migration runner + `schema_version` table.** Define an embedded, ordered list of migrations
   (each a `(version: u32, sql: &str)` or a small `Migration` struct), a `schema_version` bookkeeping
   table, and `migrate(&Db)` that applies pending migrations **idempotently** inside a transaction
   and is safe to call on every start/relaunch. Add a checksum/applied-at column so a relaunch
   re-verifies rather than re-applies. Green: unit tests over an in-memory `:memory:` DB prove
   idempotent re-run and forward-only application.
4. **System + Project schema skeletons (empty).** Ship migration #1 for each DB with the table
   *shells* later tickets fill: System DB `projects`, config; Project DB connection/config/state
   placeholders. Keep them genuinely minimal — do NOT pre-build t43/t45 columns; those arrive in
   their own migrations so each PR's schema delta is reviewable.
5. **Binary wiring (no behavior change yet).** Have `crates/qfs/src/main.rs` open the System DB via
   the seam and run `migrate()` on start, behind the binary leaf. Do NOT route any existing command
   through it yet (the file vault still backs secrets until t43). Green: binary starts, migrations
   apply once, second start is a no-op.

## Key files

- `crates/store/` (new): `Cargo.toml`, `src/lib.rs` (`Db`, `SystemDb`, `ProjectDb`, `DbSource`),
  `src/migrate.rs` (runner + `schema_version`), `src/schema/system.sql` + `src/schema/project.sql`
  (or inline migration consts).
- `crates/cmd/tests/dep_direction.rs` (modify): add `qfs-store` to the leaf/runtime allowlists.
- `crates/qfs/src/main.rs` (modify): open System DB + run migrations on start.
- `crates/qfs/Cargo.toml` (modify): depend on `qfs-store` (binary leaf only).
- `crates/qfs/Cargo.toml` version bump `0.0.7 → 0.0.8`.

## Considerations

- **Safety floor.** Migrations are the one place that mutates persistent structure; they run inside a
  single transaction so a crash mid-migration rolls back (decision E's "built fresh" must not mean
  "corruptible"). Opening a DB and running migrations is **start-time infrastructure**, not a qfs
  effect-plan — it never goes through preview/commit; it is the substrate that later `/sys/*` writes
  (t53) preview and commit *over*.
- **Dep-direction discipline.** `qfs-store` is sync (`rusqlite`) and MUST stay a leaf: tokio stays in
  `qfs-runtime` + the binary, and only the binary `crates/qfs` opens a real DB file. Adding the crate
  to `crates/cmd/tests/dep_direction.rs` is part of this slice, not an afterthought.
- **Decision F seam (do not over-build).** Design the `DbSource` so a reverse proxy can later inject
  the tenant→DB route — but do NOT implement distributed SQLite / D1 / EFS now (that is M8). The
  invariant to lock in today is *the binary never hard-codes a DB filename in a command path*; the
  source is injected.
- **Honesty first.** Do not advertise "all-SQLite persistence" in the README/skill/`docs/roadmap.md`
  status tags until a real surface uses it — this ticket ships the runner + empty skeletons, so the
  roadmap row stays 🧭/🔌, not ✅, until t43/t45 wire real data.
- **Open product decision to flag (not guess).** Where the System DB lives by default (alongside
  `~/.config/qfs/` per the current `default_credentials_path()` XDG/HOME convention, vs. a new
  `~/.local/share/qfs/`) and whether Project DBs are one-file-per-project or rows-in-System — flag
  this for the reviewer rather than baking it in; t43/t53 depend on the answer.
- **Versioning.** Own PR + patch bump (`0.0.7 → 0.0.8`) + a `v0.0.8` tag on ship, per CLAUDE.md.
