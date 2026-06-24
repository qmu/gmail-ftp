---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort:
commit_hash: e3cf50e
category: Added
depends_on: [20260622214650-t09-effect-plan-and-preview-commit.md, 20260622214650-t16-driver-local-filesystem.md]
---

# CLI: interactive FTP-like shell

## Overview

Implements the interactive half of RFD §7 (CLI): an FTP-like REPL invoked as bare `cfs`.
It gives an AI agent or human a stateful session with a *current working location* tagged
`{driver, path}` (RFD §2 VFS, §5 archetypes) and the filesystem verbs (`ls/cd/pwd/cat/cp/mv/rm`)
that are sugar over the closed-core grammar (RFD §3). The shell binds the operating procedure
of the whole system — `write statement → PREVIEW → COMMIT` (RFD §1, §6) — to an ergonomic loop:
relative paths resolve against cwd, destructive ops over a set show affected counts and require
explicit commit, and `cp` spans mounts without leaving cwd. The point is that the shell adds
**no new semantics** — it is a thin convenience layer that desugars to the same plans the
one-shot path produces, so everything stays dry-runnable and testable.

## Scope

In scope:
- The REPL loop, line editor, prompt rendering (`{driver}:{path}$ `), history.
- Session state: active account/cwd as a tagged `Location { driver, path }`; relative→absolute
  path resolution against cwd.
- Filesystem builtins `ls`, `cd`, `pwd`, `cat`, `cp`, `mv`, `rm` desugared to core statements.
- Cross-mount `cp`/`mv` (source and dest under different drivers) without `cd`-ing.
- Running raw cfs statements typed at the prompt (anything not a builtin), with inline
  `PREVIEW` (default) and `COMMIT` UX and affected-count display.
- Tab completion for builtins, mount/driver names, and path components (via driver `ls`).

Out of scope (deferred):
- One-shot `cfs run`/`-e`, `-json` output, absolute-only addressing — sibling ticket t27 (CLI one-shot runner).
- Auth/account switching, credential store — t-auth/E5 tickets (shell consumes the resolved registry).
- Non-local drivers beyond what t16 (local FS) provides; completion against remote drivers
  works via the generic `Driver` trait but only local is exercised here.
- Server `CREATE …` DDL forms and `cfs serve` — E7.

## Key components

New crate-internal module `cli::shell` (binary crate `cfs`), reusing engine crates from
t09 (effect-plan/preview/commit) and t16 (local driver). No vendor types cross the boundary
(RFD §9 owned DTOs); the shell only sees core AST, `Plan`, and DTO rows.

- `struct Session { cwd: Location, registry: Arc<DriverRegistry>, interp: Interpreter }`.
- `struct Location { driver: DriverId, path: VfsPath }` — the tagged cwd; `Display` renders the prompt.
- `fn resolve(&self, raw: &str) -> Result<VfsPath, CliError>` — relative/`..`/`~`-style resolution
  against cwd, producing an absolute `/driver/...` path; pure, unit-testable.
- `enum Builtin { Ls, Cd, Pwd, Cat, Cp, Mv, Rm }` with `fn desugar(self, args, cwd) -> Statement`
  — each builtin lowers to a closed-core `Statement` (e.g. `rm a b` → `FROM <p1> UNION FROM <p2> |> REMOVE`;
  `cp src dst` → `FROM <src> |> INSERT INTO <dst>`; `ls` → `FROM <p> |> SELECT name,size,...`).
  Builtins are **not** keywords — they are CLI-layer desugaring only (RFD §3 governance).
- `fn eval_line(&mut self, line: &str) -> Result<Outcome, CliError>` — dispatch builtin vs. raw
  statement; both paths go through the same `Interpreter` and honor capability gating (parse-time
  rejection of unsupported verbs per RFD §5 → surfaced as structured `CliError`).
- `enum Outcome { Preview(PlanSummary), Committed(ApplyReport), Listing(Vec<Row>) }`.
- `trait Completer` impl over `DriverRegistry`: completes builtin names, driver mounts, and path
  segments by issuing a cheap `ls` (pure read) against the resolved parent.
- Line editing via `rustyline` (or `reedline`); completion + history + prompt wired through it.
- `PlanSummary` / preview rendering reused from t09; the shell only formats it.

## Implementation steps

1. Add `cli::shell` module and the `cfs` binary entrypoint that drops into the REPL when no subcommand/args.
2. Define `Location`, `Session`, and `VfsPath` resolution (`resolve`) with unit tests for
   relative, `..`, root, and cross-driver absolute cases.
3. Wire the line editor (`rustyline`): prompt from `Location::Display`, persistent history file
   under the cfs config dir.
4. Implement builtin parsing (split line → builtin + args) and `Builtin::desugar` to core `Statement`s.
5. Route every line through one `eval_line`: builtins desugar then run; non-builtin lines parse as
   raw cfs statements. Both call `Interpreter::plan(stmt)`.
6. Implement PREVIEW-by-default: print `PlanSummary` + affected counts; require explicit `COMMIT`
   (typed, or `--commit`/confirmation) before applying. Destructive set ops always show counts first.
7. Implement `cd`/`pwd` to mutate/print `Session.cwd` (no plan; pure state change), validating the
   target exists/is a namespace via a driver capability check.
8. Implement cross-mount `cp`/`mv`: resolve src & dst independently against cwd; desugar to a
   cross-source plan (copy→verify→delete for `mv`, per RFD §6 recovery), without changing cwd.
9. Implement the `Completer`: builtin names, mount list from registry, path segments via `ls`.
10. Map all errors to structured `CliError` with actionable messages (esp. capability/parse errors).
11. Golden tests: feed scripted input lines, assert emitted plans and rendered output.

## Considerations

- **Purity / effects-as-data (RFD §3, §6):** builtins must desugar to plans and route through
  the same `Interpreter`; the shell performs no I/O of its own except `ls`-for-completion (pure reads)
  and `cd` validation. Never let a builtin shortcut around PREVIEW/COMMIT — that is the safety invariant.
- **Capability gating & least privilege (RFD §5, §10):** unsupported verbs on a node are rejected
  before any effect; the shell surfaces the structured error rather than half-applying. `rm`/`mv`
  are irreversible — always preview with counts and require explicit commit.
- **Idempotency / recovery (RFD §6):** cross-mount `mv` lowers to copy→verify→delete; on partial
  failure the audit ledger is authoritative — the shell reports the ledger state, does not silently retry.
- **Hard parts:** (a) relative-path resolution across the `/driver/...` boundary (a `cd` cannot cross
  into a driver that has no namespace archetype — gate via capabilities); (b) completion latency —
  cache the parent `ls` per prompt and bound it with a short timeout so a slow driver never hangs the REPL;
  (c) distinguishing a builtin from a raw statement unambiguously (reserve builtin names only at the
  line head, everything else parses as cfs). Resolve (c) by lexing the first token and checking the
  builtin set before falling through to the grammar.
- **Observability:** every committed plan goes to the audit ledger (RFD §6) with the session as origin.
- **Standards:** keep `cli::shell` free of vendor/driver internals (owned DTOs only); no secrets are
  read or printed by the shell; history file excludes nothing sensitive because the shell never handles creds.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; `cargo test` passes.
- Launching `cfs` with no args enters the REPL; prompt shows `{driver}:{path}$` reflecting cwd.
- `cd`/`pwd`/`ls` against the local driver (t16) navigate and list correctly; `cd` into a
  non-namespace node is rejected with a structured error.
- `rm`/`mv`/`cp` over a set print affected counts and a PREVIEW by default; nothing is applied
  until an explicit COMMIT — asserted by **plan assertions** (the produced `Plan` DAG) in tests,
  not live effects.
- Cross-mount `cp src /other-driver/dst` produces a cross-source plan without changing cwd
  (asserted via golden plan snapshot).
- A raw cfs statement typed at the prompt produces the same plan as the equivalent one-shot input.
- Tab completion returns builtin names, mount names, and path segments; tests cover completion
  resolution without live credentials (local driver only).
- Golden tests over scripted REPL sessions assert rendered output and emitted plans.
