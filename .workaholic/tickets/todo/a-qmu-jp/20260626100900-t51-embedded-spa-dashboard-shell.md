---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort:
commit_hash:
category:
depends_on: [20260626100500-t47-mcp-server-binding-tools.md]
---

# t51 — Embedded SPA dashboard shell over the same engine

## Overview
Delivers the **second of the three faces** (roadmap §"One engine, three faces", M3): a static
single-page dashboard compiled into the `qfs` binary and served by the existing in-house HTTP
listener (`crates/http`). This is the *shell* only — an authenticated page that composes qfs
statements in the browser and calls the SAME engine path the CLI and MCP face already use
(`describe` → `preview` → `commit`); the rich preview/commit cards are t52 and the `/sys/*` admin
views are t53. The one rule it enforces from day one is the constraint the roadmap names: the
dashboard may expose **no capability the CLI/MCP lack**, because it composes the same qfs statement
and runs it through the same `qfs_exec` read/apply path. What already exists as a library: the whole
HTTP serving stack (`crates/http` `serve_config_full`, the `Fallback` seam, `HttpBinding` route
table) and a credential-free built-in read source pattern (`crates/qfs/src/serve_builtins.rs`
`/status`). What is genuinely new: the embedded asset bundle, a static-asset route on the listener,
and a thin JSON bridge endpoint that forwards a posted qfs statement into `qfs_exec`. There is NO
existing SPA, asset embedding, or browser bridge anywhere today.

## Exact seams
- `crates/http/src/serve.rs` — `serve_config_full(...)` already takes an optional
  `Fallback = Arc<dyn Fn(&HttpRequest) -> Option<HttpResponse>>` (line 67/95/102); the watchtower
  webhook ingest rides it (`run_serve` passes `Some(wt_fallback)` at line 129). The static SPA
  shell rides the SAME fallback seam (or a second composed closure) — no new server primitive.
  `DEFAULT_BIND_ADDR = "127.0.0.1:8787"` (line 35), overridable via `QFS_HTTP_ADDR`.
- `crates/http-core/src/lib.rs` — pure `HttpRequest`/`HttpResponse`/`HttpMethod` DTOs +
  `SENSITIVE_HEADERS`/`is_sensitive_header`. The shell builds responses from these owned DTOs
  (correct `Content-Type`, cache headers); no vendor HTTP types leak.
- `crates/http/src/binding.rs` `HttpBinding` — the hot-swapped route table for *declared*
  `CREATE ENDPOINT` routes. The SPA is NOT a declared endpoint (it is binary-static), so it lands
  on the `Fallback`/static path, leaving the `HttpBinding` registry untouched. Mirror its
  "immutable snapshot, no lock across `.await`" discipline.
- `crates/qfs/src/serve_builtins.rs` — `STATUS_MOUNT = "/status"`, the self-contained credential-
  free built-in read source + no-op applier. This is the existing PATTERN for "served by the binary,
  no network, no creds" that the static-asset handler imitates (a read-only, in-process source).
- `crates/exec/` — `qfs_exec::{build_plan, execute_read}` and `run_oneshot`/`apply_commit`: the
  ONE engine path. The browser bridge calls exactly these, the same as `qfs run` and the t47 MCP
  `preview`/`commit` tools. No second executor.
- `crates/qfs-mcp` (NEW, from t47) — the JSON bridge SHOULD reuse t47's request→engine mapping so
  the dashboard and the MCP face share one statement-execution adapter rather than forking it.
- `crates/qfs/src/serve.rs` `run_serve` (line 22) — the composition root that wires
  `CronBinding` + `WatchtowerBinding` + the webhook fallback. The static-asset route and bridge are
  injected here (binary leaf only, per dep-direction).
- Session gate: `t46` session-cookie core (System DB sessions) — the shell is served behind it when
  identity is on. Until t46 lands, the shell binds to loopback only (the existing default).
- `crates/cmd/tests/dep_direction.rs` — if a new `qfs-dashboard` asset crate is added, it must be
  registered in the leaf allowlist; assets+bridge wiring otherwise lives in the `qfs` leaf.

## Implementation steps
1. **Asset bundle (tree stays green).** Add a minimal static SPA (`index.html` + one JS + one CSS,
   self-contained, no external CDN) under `crates/qfs/assets/dashboard/` (or a new `qfs-dashboard`
   leaf added to `dep_direction.rs`). Embed via `include_bytes!`/`include_str!` (no build-time
   network). Slice ships a blank "qfs dashboard" shell that loads — `cargo build/test/clippy/fmt`
   and `gen-docs --check` green.
2. **Static-asset handler.** Add a function in `crates/qfs/src/serve.rs` (or `serve_builtins.rs`
   sibling) returning `Option<HttpResponse>` for `GET /` and `GET /assets/*`, built from
   `http_core::HttpResponse` with correct `Content-Type` and immutable-asset cache headers. Compose
   it with the existing `wt_fallback` into the single `Fallback` passed to `serve_config_full`.
3. **Engine bridge endpoint.** Add a `POST /api/run` (read/preview) handler on the same fallback
   that decodes `{ statement, mode }`, calls `qfs_exec::build_plan` then `execute_read` (preview-
   only in this slice — NO commit yet; commit is t52's gated card), and returns plan/rows as JSON
   via the codec registry. Reuse t47's adapter so MCP and the dashboard share one path.
4. **Session gate seam.** Behind a flag/feature, require a valid session cookie (t46) on `/` and
   `/api/*`, returning `401` + a redirect to a sign-in page when identity is enabled; loopback-only
   default when it is not. Leave a documented hook for t46 rather than inventing auth here.
5. **Docs honesty + version.** Note in `docs/roadmap.md` status tags only after the shell loads and
   serves a preview; do NOT advertise commit/admin views (t52/t53). Bump patch in
   `crates/qfs/Cargo.toml`.

## Key files
- NEW `crates/qfs/assets/dashboard/{index.html,app.js,app.css}` (or a `crates/qfs-dashboard` leaf).
- `crates/qfs/src/serve.rs` — compose static + bridge into the `Fallback`; wire in `run_serve`.
- NEW `crates/qfs/src/dashboard.rs` — asset handler + `/api/run` bridge (built on `qfs_exec`).
- `crates/cmd/tests/dep_direction.rs` — add any new leaf to the allowlist (if a crate is created).
- `crates/qfs/Cargo.toml` — patch bump.
- `docs/roadmap.md` — flip the M3 "embedded SPA" status tag once it loads (honesty rule).

## Considerations
- **Safety floor inherited, not re-implemented.** The shell MUST go through `qfs_exec` so describe
  stays pure, preview touches nothing, and commit stays explicit + gated. This slice serves
  **preview/read only**; the commit button and the irreversible acknowledgement are t52. Do not add
  a shortcut commit path in the bridge — that would break the one-engine constraint.
- **Untrusted input seam.** `/api/run` takes a browser-supplied statement string; it is parsed and
  planned through the normal pipeline (no string-splicing into anything), and the response is built
  from `http-core` DTOs with `is_sensitive_header` redaction. Never echo credentials or raw upstream
  errors; sanitize engine errors into a machine-readable JSON problem body (mirror `crates/http`
  `error.rs` mapping).
- **Self-contained assets.** No external fonts/CDN/scripts — the binary is the deliverable
  (CLAUDE.md: published Release is the product). Embedded assets keep `qfs serve` offline-clean and
  match the hermetic-test rule.
- **Dep-direction.** Asset embedding + bridge wiring is live-runtime glue → it lands on the `qfs`
  binary leaf (where tokio dead-ends), never in `qfs-cmd`/lang/plan. A new asset crate must be added
  to `TERMINAL_LEAVES`-adjacent allowlists in `dep_direction.rs`.
- **Session/identity ordering.** This depends on t47 (the engine-over-HTTP face) but the session
  gate depends on t46; if t46 is not yet shipped, ship loopback-only and flag the gate as a follow-
  up rather than rolling a bespoke auth.
- **Open product decision to flag (not guess):** the SPA's framework/build toolchain (hand-written
  vanilla vs. a bundler) and whether assets are embedded in `qfs` or a sibling crate — pick the
  lightest path that keeps the binary self-contained; record the choice in the PR rather than baking
  a heavy frontend toolchain into the workspace.
- **Versioning.** One PR, patch bump in `crates/qfs/Cargo.toml`, `v0.0.x` tag on ship.
