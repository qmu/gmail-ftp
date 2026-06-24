---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort:
commit_hash: c13e2c8
category: Added
depends_on: [20260622214650-t09-effect-plan-and-preview-commit.md]
---

# CLI: one-shot execution + output formats

## Overview
Delivers the non-interactive face of the `qfs` binary described in RFD §7: `qfs run '<stmt>'`
(and `-e <stmt>`) executes a single pipe-SQL statement with **no cwd** — addressing is by
absolute VFS path or `id:`/path form only — and renders results as either machine `--json`
or a human `table`. This is the surface AI agents drive: a stable, scriptable contract with a
typed `{"error": …}` body + non-zero exit on failure, and the PREVIEW/COMMIT safety gate
(RFD §6, §7) made explicit for destructive ops over a set (show counts, require `COMMIT`/
`--commit`). It implements §7 bullets 2–3 and surfaces the effect-plan engine from ticket
t09 (PREVIEW default) through the CLI. The interactive shell (§7 bullet 1) is a sibling.

## Scope
In scope:
- `qfs run '<stmt>'` and `qfs -e '<stmt>'` subcommand/flag parsing (one statement per invocation).
- Stdin statement source (`qfs run -` reads stmt from stdin) for agent pipelines.
- Output renderers: `--format json|table` (alias `--json`), default `table` on a TTY, `json` when piped.
- Stable error contract: serialize engine/parse errors to `{"error": {...}}` JSON on stderr + non-zero exit codes.
- PREVIEW-by-default; `--commit` (and recognizing a trailing `COMMIT`) to apply; count summary for set-wide destructive plans.
- Exit-code map and `--quiet`/verbosity plumbing.

Out of scope (deferred):
- Interactive FTP-like shell, cwd, completion → sibling t28 (CLI: interactive shell).
- The effect-plan/PREVIEW/COMMIT engine internals, batching, audit ledger → **t09** (dependency).
- `qfs serve` and `/server/...` bindings → E7 server tickets.
- Auth/credential resolution → E5; this ticket consumes the credential context, does not build it.
- Driver registration and capability tables → E4 / E1; consumed read-only here.

## Key components
New crate `qfs-cli` (binary `qfs`), thin over the core `qfs-engine` crate (from t09/E1–E2):
- `cli::args` — `clap` derive. `Cli { command: Command }`; `Command::Run(RunArgs)`.
  `RunArgs { stmt: Option<String>, expr: Option<String> /*-e*/, format: OutputFormat,
  commit: bool, quiet: bool }`. Mutually-exclusive `stmt`/`-e`/stdin resolved into one source.
- `cli::run` — `fn run_oneshot(src: StmtSource, ctx: &EngineCtx, fmt: OutputFormat, commit: bool) -> ExitCode`.
  Pipeline: parse → typecheck/capability-resolve → build `Plan` → if no effects or `!commit`
  render PREVIEW; else `engine.commit(plan)` → render result. Never touches a cwd.
- `cli::output` — `trait Renderer { fn rows(&self, &RowSet, &mut dyn Write); fn plan(&self, &PlanPreview, …); fn error(&self, &CfsError, …); }`
  with `JsonRenderer` and `TableRenderer` (uses `comfy-table`/owned formatting; no vendor types).
  Renders **owned DTOs only** (`RowSet`, `PlanPreview`, `CfsError`) — SDK/vendor types never reach the CLI (RFD §9).
- `cli::error` — `CfsError` (already core) → stable JSON `{"error":{"kind","message","path?","detail?"}}`;
  `ExitCode` enum: `0` ok, `2` parse/usage, `3` capability/unsupported-op, `4` commit-required,
  `5` effect/commit failure, `6` auth/credential. Kept stable for agents.
- `cli::addressing` — validates absolute-path / `id:` addressing; rejects relative paths with a
  structured `usage` error (no cwd exists in one-shot mode).

Respected invariants: closed-core grammar (CLI adds **zero** keywords — `COMMIT` is parsed by
the engine, `--commit` is only the apply switch); three open registries untouched; effects-as-data
(CLI renders a `Plan`, the engine alone commits — purity invariant held at the boundary);
capability gating surfaced as exit `3`; owned DTOs across the engine↔CLI seam.

## Implementation steps
1. Scaffold `qfs-cli` crate; wire `main()` → `clap` `Cli::parse()` → dispatch `Command::Run`.
2. Implement `StmtSource` resolution (positional stmt | `-e` | `-` stdin); error on >1 source.
3. Implement absolute/`id:` addressing validation; emit structured usage error for relative paths.
4. Call core parse + capability resolution; map every error into `CfsError` with a `kind`.
5. Build the `Plan` via the t09 engine; detect "has effects" and `irreversible` flags.
6. Default to PREVIEW: render plan + per-target affected counts; if destructive set and `!commit`,
   exit `4` with a "re-run with --commit (or trailing COMMIT)" message.
7. On `--commit`/trailing `COMMIT`, call `engine.commit(plan)`; stream/collect applied result.
8. Implement `JsonRenderer` (stable schema: `{rows|plan|error}`) and `TableRenderer` (TTY-aware).
9. Default format by `IsTerminal`: `table` on TTY, `json` when piped; `--json`/`--format` override.
10. Map outcomes to `ExitCode`; ensure errors go to stderr, data to stdout.
11. Golden tests + `--help` snapshot; document the exit-code/JSON contract in `--help` and README.

## Considerations
- **Least-privilege & secrets** (RFD §10): the CLI never logs or echoes credential material; the
  error DTO is whitelisted-field (no token/path leakage); `--quiet` suppresses progress, never
  swallows the error body.
- **Idempotency / recovery** (RFD §6): destructive/irreversible plans (`REMOVE`, `CALL mail.send`)
  are gated behind explicit commit + count display; partial-failure on cross-source commit returns
  exit `5` with the applied-effect summary so an agent can reconcile from the ledger. CLI performs
  no retry itself — that is engine policy.
- **Observability**: structured logs via `tracing` to stderr (`-v`/`RUST_LOG`); machine JSON stays
  pristine on stdout. PREVIEW output is the human/CI dry-run artifact.
- **Hard parts**: (a) the PREVIEW→COMMIT UX must stay a pure render of an engine `Plan` so purity
  holds — resolve by rendering only, never mutating; (b) stable JSON/exit contract for agents —
  resolve by pinning schemas in golden tests so any drift fails CI; (c) "destructive over a set"
  detection must come from the plan's effect/`irreversible` metadata, not from CLI keyword sniffing,
  to stay grammar-agnostic; (d) TTY-vs-pipe default must be deterministic for scripts (explicit flag always wins).
- **Standards**: thin CLI crate, no business logic; vendor types excluded by construction.

## Acceptance criteria
- `cargo build`, `cargo clippy -- -D warnings`, `cargo test` green; `qfs --help`/`qfs run --help` snapshot-stable.
- `qfs run 'FROM /mail/inbox |> LIMIT 1' --json` prints `{"rows":[…]}` to stdout, exit `0` (against a fake/in-memory driver — **no live creds**).
- A pure query renders as a `table` on a TTY and `json` when piped, with no plan/commit prompt.
- A statement with effects but no `--commit` prints a PREVIEW with affected counts and exits `0` (preview) ; a destructive set-wide plan without commit exits `4`.
- `qfs run '<bad syntax>'` writes `{"error":{"kind":"parse",…}}` to stderr and exits `2`; an unsupported-op (capability) case exits `3`.
- A relative-path address is rejected with a structured `usage` error (exit `2`); absolute and `id:` forms accepted.
- **Plan assertions**: golden tests assert the serialized `Plan`/`PlanPreview` for representative effect statements (preview path) without committing; commit path tested only against the in-memory engine.
- Exit-code map and JSON error schema documented and covered by tests.
