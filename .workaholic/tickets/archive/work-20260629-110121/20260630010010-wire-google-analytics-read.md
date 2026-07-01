---
created_at: 2026-06-30T01:00:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, UX]
effort: 4h
commit_hash: bbeb653
category: Added
depends_on: []
---

# Wire `/ga` reads for real (query → GA report) and decide the `/ga` → `/analytics` rename

Roadmap "Near-term backlog": Google Analytics reads aren't wired — `/ga` returns the honest
*"connect a Google account"* error even when connected. GA returns **pre-aggregated** metrics
(grouped by dimensions over a mandatory date range), not a plain table, so the read facet must map a
query to a GA report request. Deferred alongside Drive in
`archive/work-20260629-110121/20260629140100-wire-gmail-gdrive-ga-read-rows.md`.

## What's missing (confirmed)

- `crates/qfs/src/shell.rs:323` registers `/ga` to the `ConnectAccountReadDriver` stub; nothing
  layers a live GA reader over it. No `GaReadDriver` in `crates/qfs/src/read_facets.rs`.
- The pieces to compose **already exist and are mock-tested**, just unbridged: `GaDriver::fetch_catalog`
  (`crates/driver-ga/src/lib.rs:163`), `compile::compile` (query → `RunReportRequest`,
  `crates/driver-ga/src/compile.rs`), `run_report` (`lib.rs:172`), `response_to_rows`
  (`crates/driver-ga/src/report.rs`).

## Plan

1. Add a `GaReadDriver` adapter in `read_facets.rs`. Unlike the other facets it is **two-step**:
   `fetch_catalog` (property schema) → `compile` the pushed query → `run_report` → `response_to_rows`.
2. Surface `GaError::MissingDateRange` / `UnknownField` as actionable read errors (a GA query needs a
   date range; a property is addressed by numeric id — `/ga/<propertyId>` or `/ga/<propertyId>/realtime`).
3. Register over the `/ga` fallback in `shell.rs:320-349`.

## The `/ga` naming decision (roadmap aside)

The roadmap notes `/ga` is "a cryptic name — it should be spelled out, or renamed to `/analytics`."
Decide during this ticket. If renaming: the mount string lives in **one** place,
`crates/driver-ga/src/path.rs:22` (`pub const MOUNT: &str = "/ga"`); the runtime driver id `"ga"` is
also stamped at `crates/qfs/src/shell.rs:323`, `crates/qfs/src/commit.rs:494`, and
`crates/qfs/src/google.rs:124` (consent scopes). Renaming is a **versioned path-surface change** — if
adopted, keep `/ga` as a deprecated alias for one release rather than a hard break. Default
recommendation: rename to `/analytics` while wiring the read (single coherent PR), `/ga` aliased.

## Key files

- `crates/qfs/src/read_facets.rs`, `crates/qfs/src/shell.rs:320-349`.
- `crates/driver-ga/src/{lib.rs:163,172,compile.rs,report.rs,path.rs:22}`.

## Considerations

- Bump the patch in `crates/qfs/Cargo.toml`; regenerate `docs/drivers.md` via `xtask gen-docs` if the
  mount name or surface changes (anti-drift). Sibling of `20260630010000` (Drive read).
