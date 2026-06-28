---
created_at: 2026-06-26T10:23:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: L
commit_hash: e188846
category: Changed
depends_on: []
---

# t65 — Externalize scheduling (OS cron + Cloudflare Cron Triggers); retire the internal scheduler

## Overview

Implements roadmap **decision M (revised)** and **§4.3**: **qfs is not a scheduler.** Instead of
building an internal cron daemon and, at scale, System-DB leader election so a job fires exactly once,
qfs externalizes the *when* and keeps only the *what* — an invokable unit (`qfs run '<stmt>' --commit`,
a saved named plan, or an `ENDPOINT`) that an external scheduler drives:

- **Individual / local** — OS `cron` (or `launchd` / systemd timers / Task Scheduler) runs a
  `qfs run` line on its schedule. No qfs daemon, no qfs-side scheduler state.
- **Managed tier** — **Cloudflare Cron Triggers** (`[triggers] crons = […]` in wrangler) fire the
  qfs Worker on schedule; the platform owns distribution + exactly-once.

This **supersedes** the old t65 (System-DB lease leader election). The Cloudflare deployment already
maps this way — `crates/host/src/workers.rs` maps `JOB → scheduled/Cron Triggers`. The work here is
to (a) **remove the native cron daemon / leader-election plan** from the running path, (b) make the
external-invocation surface clean (a stable, named, re-runnable unit), and (c) document both paths.

## Exact seams

- `crates/cron/` — today the **native** scheduler: `src/daemon.rs` (`run_daemon`, the tokio interval
  loop), `src/binding.rs` `CronBinding`, `src/store.rs` `JobStore` (`get/poll/mark_run/lease`),
  `src/scheduler.rs` (`run_id_for`). Under externalization the **native daemon path is retired**; the
  pure schedule math is no longer needed on the running path. Decide: delete `crates/cron`, or shrink
  it to just the cron-expression *parser* if Cloudflare wrangler generation needs to validate
  `crons = [...]`. The `lease`/leader-election extension is dropped entirely.
- `crates/host/src/workers.rs` — already maps `JOB → scheduled/Cron`. KEEP/finish this: a `JOB`'s
  `EVERY` interval becomes a wrangler **Cron Trigger** entry, not a qfs-run daemon tick.
- `crates/host/src/wrangler.rs` `generate_wrangler_toml` — emit the `[triggers] crons` block from the
  server's `JOB` rows so the managed deployment's schedule is the platform's, generated from qfs config.
- `crates/qfs/src/serve.rs` (`run_serve`) + `crates/qfs/src/cron.rs` — **stop wiring `CronBinding` +
  the daemon loop** into the native serve path. `serve` no longer runs a scheduler thread; an
  EC2/individual deployment is driven by OS cron calling `qfs run` instead.
- `crates/core/src/ddl/server.rs` `server_node_schema(/server/jobs)` — a `JOB` row stays as a **saved
  named plan + its intended cadence** (so wrangler/host can read it and so a human can `qfs run` it by
  name); it is metadata for the *external* scheduler, no longer something qfs fires itself. (`JOB`/
  `EVERY`/`DO` remain frozen DDL keywords — no keyword change.)
- `crates/cmd/tests/dep_direction.rs` — drop `qfs-cron` from the binary's wiring if the crate is
  removed; keep the allowlists consistent.
- Docs: `docs/cookbook/automation.md`, the query cookbook's automation recipes, `docs/server.md`
  (generated) — reframe `CREATE JOB … EVERY …` as "a saved plan an external scheduler runs," with the
  OS-cron and Cloudflare-Cron-Triggers invocation shown.

## Implementation steps

1. **Remove the running scheduler.** Stop composing `CronBinding`/`run_daemon` in
   `crates/qfs/src/serve.rs`/`cron.rs`; `qfs serve` no longer spawns a scheduler. Tree green; existing
   cron tests that assert in-process firing are deleted or converted to wrangler-generation tests.
2. **Managed path — generate Cron Triggers.** In `crates/host` (`from_server`/`wrangler.rs`), turn each
   `/server/jobs` row's `EVERY` into a `[triggers] crons` entry in the generated `wrangler.toml`, and
   route the Worker's `scheduled` event to the saved plan's commit path. Verify the generated TOML.
3. **Individual path — invokable unit + docs.** Confirm `qfs run '<stmt>' --commit` (and a
   `qfs run --job <name>` that loads a saved `/server/jobs` plan by name) is a clean, non-interactive,
   exit-code-correct command suitable for a crontab line. Write the OS-cron how-to (crontab example,
   `QFS_PASSPHRASE`/env-cred note, `--commit-irreversible` caveat).
4. **Retire `crates/cron` (or shrink).** Delete the daemon/scheduler/lease; keep only a cron-expression
   validator if wrangler generation needs it. Update `dep_direction.rs`.
5. **Docs honesty + version.** Reframe every "qfs schedules" claim (roadmap is already updated; do the
   generated `docs/server.md`, cookbook automation, guide). Patch-bump `crates/qfs/Cargo.toml`;
   `cargo run -p xtask -- gen-docs --check` green.

## Key files

- `crates/qfs/src/{serve.rs,cron.rs}` (stop wiring the daemon).
- `crates/host/src/{workers.rs,wrangler.rs,from_server.rs}` (JOB → Cron Triggers generation).
- `crates/cron/*` (delete or shrink to a cron-expr validator), `crates/cmd/tests/dep_direction.rs`.
- `crates/core/src/ddl/server.rs` (`/server/jobs` row stays as saved-plan metadata).
- `docs/cookbook/automation.md`, `docs/query-cookbook.md`, generated `docs/server.md`; the OS-cron how-to.
- `crates/qfs/Cargo.toml` (patch bump).

## Considerations

- **Why externalize.** Owning a scheduler means owning a daemon, missed-run policy, and — at scale —
  leader election so a job fires once, not once per instance. OS cron and Cloudflare Cron Triggers
  already solve this robustly. Externalizing deletes that whole stateful surface (and the System-DB
  lease) from qfs; qfs stays stateless and the platform owns the hard part.
- **Safety floor unchanged.** Whatever fires the unit, it still runs through PREVIEW→COMMIT under its
  `POLICY`; irreversible effects in a scheduled plan still need their acknowledgement via the safety
  mode. An external trigger does not bypass the gate — it just decides *when*.
- **`JOB`/`EVERY` keep their meaning, lose their daemon.** A `JOB` is now a saved named plan plus an
  intended cadence that an external scheduler reads/runs; qfs does not tick it. No keyword change
  (closed core untouched). If the team later wants to drop `JOB`/`EVERY` entirely (express schedules
  purely in crontab/wrangler), that is a separate keyword-reduction decision.
- **Idempotency still matters.** External schedulers are at-least-once (a Cron Trigger can double-fire
  on retry). Keep effects idempotent — `UPSERT` / `@version` preconditions — so a re-fire is a no-op;
  this is the same discipline the old internal ledger provided, now the author's responsibility,
  documented in the how-to.
- **Versioning:** own PR + patch bump + `v0.0.x` tag on ship (CLAUDE.md).
