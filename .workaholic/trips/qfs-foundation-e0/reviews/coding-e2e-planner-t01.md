# Coding Phase E2E Review — t01 CLI (Planner)

Author: Planner (Progressive)
Type: E2E / external-interface testing (build-and-run from the outside)
Target: `qfs` CLI scaffold (ticket t01)
Date: 2026-06-22
Binary under test: `target/debug/qfs` (`qfs 0.0.0`), built via `cargo build -p qfs`

---

## Scope and method

External, black-box validation only — no code review, no unit tests (those belong to
Architect and Constructor). I built the binary and exercised it from the shell, capturing
exact stdout/stderr and exit code per scenario, then asserted: (a) no Rust panic /
backtrace noise, (b) correct exit codes, (c) under `--json`, a machine-parseable JSON error
envelope. The `--json` machine-readability is the headline business promise to Persona B
(the AI agent) per direction-v1 §3 — I validated it as a first-class contract, not a nicety.

The build was clean (no compile step needed; already built). The CLI prints a coherent
usage banner that lists both `run` and `serve` plus the global `--json`/`-h`/`-V` options
and documents the no-subcommand interactive-shell behavior.

## Observed subcommand contract (from `--help`)

- `qfs run --stmt/-e <STMT>` — statement is a **required option**, no positional form.
- `qfs serve <CONFIG>` — config is a **positional argument**.
- `qfs` (no subcommand) — interactive FTP-like shell (currently stubbed).
- Global `--json` accepted both before the subcommand (`qfs --json run …`) and after
  (`qfs run … --json`); both produce a byte-identical JSON envelope.

## Structured-error envelope (the AI-agent contract)

Human form (stderr): `error[<code>]: <message>` — e.g. `error[not_implemented]: not yet implemented: run`

JSON form (stderr), confirmed parseable by **both** `python3 json.load` and `jq .`:

```json
{
  "error": {
    "code": "not_implemented",
    "message": "not yet implemented: run"
  }
}
```

Parsed structure: top-level key `error`; nested keys `code` (string, `not_implemented`)
and `message` (string, identifying the unimplemented surface: `run` / `serve` / `shell`).
Stable and predictable across all three command surfaces — exactly the machine-legible,
self-correction-friendly shape Persona B needs.

---

## Per-scenario results

| # | Scenario | Command | Exit | Result |
|---|----------|---------|------|--------|
| 1a | version | `qfs --version` | 0 | **PASS** — prints `qfs 0.0.0` |
| 1b | version short | `qfs -V` | 0 | **PASS** — prints `qfs 0.0.0` |
| 1c | help | `qfs --help` | 0 | **PASS** — usage lists `run`, `serve`, `help`; global `--json`; documents no-subcommand shell. Coherent. |
| 2 | run pipe expr | `qfs run -e 'FROM /mail \|> WHERE x \|> SELECT y'` | 1 | **PASS** — `error[not_implemented]: not yet implemented: run` on stderr; empty stdout; no panic. |
| 3 | run positional | `qfs run 'FROM /mail'` | 2 | **PASS (with observation)** — positional form is **not a supported form**; `run` requires `--stmt`/`-e`. Result is a clean clap usage error (`error: unexpected argument 'FROM /mail' found` + usage), exit 2, no panic. Correct rejection, but see Concern 1. |
| 4 | json error envelope | `qfs --json run -e 'FROM /mail'` | 1 | **PASS** — valid JSON envelope on stderr; **confirmed parseable by python3 AND jq**; exit 1. Subcommand-position `--json` byte-identical. |
| 5 | serve nonexistent | `qfs serve /tmp/nonexistent.qfs` | 1 | **PASS** — `error[not_implemented]: not yet implemented: serve`, exit 1, no panic. `--json` form also valid (`code: not_implemented`, `message: not yet implemented: serve`). Note: stub short-circuits before any file-existence check, which is correct for a not-implemented surface. |
| 6 | no-args shell stub | `printf '' \| qfs`, `qfs < /dev/null`, `qfs --json < /dev/null` | 1 | **PASS** — does **not** hang/block on stdin; stubs cleanly with `error[not_implemented]: not yet implemented: shell` (JSON: `code: not_implemented`, `message: not yet implemented: shell`), exit 1, no panic. Guarded with a 10s timeout; returned immediately. |
| 7 | bogus subcommand | `qfs bogus` | 2 | **PASS** — clap usage error `error: unrecognized subcommand 'bogus'` + usage hint, exit 2, no panic. |

**Panic/backtrace scan:** grep across every captured stdout/stderr for
`panic`, `RUST_BACKTRACE`, `thread '`, `stack backtrace`, `unwrap`, `note: run with` —
**CLEAN, zero matches.** No scenario produced a Rust panic or backtrace noise.

**Exit-code convention observed (consistent and correct):**
- `1` = structured runtime error (`not_implemented`) — the headline scaffold behavior.
- `2` = clap argument/usage error (unknown subcommand, missing/unexpected arg).
- `0` = informational success (`--help`, `--version`).

This is the conventional Unix/clap split and is exactly what an agent or shell harness
expects: a non-zero exit on every non-success path, with `1` cleanly distinguishing a
real (structured) failure from `2`'s argument-shape error.

---

## Concerns and proposals (Critical Review Policy)

**Concern 1 — `--json` does not cover clap usage errors (S3, S7).**
The `not_implemented` envelope is JSON-clean, but argument/usage errors (exit 2) are still
emitted as clap's human-readable prose on stderr even under `--json`. For Persona B, an
agent that mis-forms a command (e.g. guesses a positional `run` form, S3) receives
unstructured prose it must pattern-match instead of parse. This is a *foundation-level*
agent-surface gap of exactly the kind direction-v1 §2 ("governance/agent-hostile surface
inherited by every later ticket") warns about.
*Proposal (business outcome: a uniform machine-legible error contract):* in a near-term
follow-up ticket, wrap clap so that under `--json` even usage errors are rendered into the
same `{"error":{"code","message"}}` envelope (e.g. `code: "usage"` / `"unrecognized_subcommand"`).
Not a t01 blocker — the scaffold's *runtime* errors are correctly structured — but worth a
tracked carry-over so the agent contract is total, not partial, before drivers land.

**Concern 2 — positional `run` ergonomics (S3).**
The RFD's interactive examples read like `FROM /mail/inbox |> SELECT subject`; requiring
`-e`/`--stmt` for one-shot `run` is a small ergonomic seam that a human (Persona A) or agent
may trip over (S3 shows the natural positional guess is rejected).
*Proposal:* consider accepting the statement as an optional trailing positional on `run`
(aliasing `--stmt`) in a later language-core ticket, so `qfs run 'FROM /mail'` works. Pure
ergonomics, explicitly **not** a t01 concern — recording it so the decision is deliberate.

Both concerns are forward-looking carry-overs, not defects in the t01 scaffold. The t01
promise — *every surface returns a structured, non-panicking, correctly-exit-coded error,
machine-readable under `--json`* — is fully met.

---

## Overall verdict

**E2E APPROVED.**

All seven scenario groups pass. The scaffold delivers the headline business promise of
direction-v1 §3 for Persona B: every command surface (`run`, `serve`, no-arg `shell`) returns
a **structured `not_implemented` error**, never a panic or backtrace; exit codes are correct
and conventional (1 = structured error, 2 = clap usage, 0 = info); and the `--json` envelope
is **confirmed machine-parseable by both python3 and jq** with a stable
`{"error":{"code","message"}}` shape. The no-arg shell stub does not hang. Help is coherent
and advertises both `run` and `serve`.

Two forward-looking carry-overs (JSON-wrap clap usage errors; consider positional `run`)
are recorded for later tickets — neither blocks t01.
