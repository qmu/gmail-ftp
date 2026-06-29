---
created_at: 2026-06-30T01:01:40+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# Color in the terminal (TTY-only, honoring `NO_COLOR` and `--no-color`)

Roadmap "Onboarding & polish": command output is all one color and hard to scan. Add color to table
headers, previews, the irreversible-action marker, and errors — **only** when writing to a terminal,
and honoring the standard `NO_COLOR` env var and a `--no-color` flag.

## Current state (confirmed)

`crates/exec/src/output.rs` is "TTY-aware" only for table-vs-json format selection — there is **no
color anywhere** (grep: ANSI hits are test-only). The repo has an explicit anti-heavy-dep stance
(`output.rs:5-12` rejects comfy-table; ADR-0002/0003 precedent), so **hand-roll ANSI escapes** rather
than add `owo-colors`/`termcolor`.

## Plan

1. Compute a single `color: bool` decision in `crates/qfs/src/cmd/lib.rs`: stdout `is_terminal()`
   (the `std::io::IsTerminal` machinery already exists at `cmd/lib.rs:690/704/750/752`) **AND**
   `NO_COLOR` unset **AND** `--no-color` not passed. Add `--no-color` to the global `Cli` struct
   (`cmd/lib.rs:222-229`, beside `--json`).
2. Thread the flag into the renderer — the `Renderer` trait (`output.rs:26`) /
   `OutputFormat::renderer()` (`output.rs:67`) currently take no config; add a color field/param to
   `TableRenderer`. Colorize: table header row + rule (`render_table`/`write_row`, `output.rs:351-393`)
   and `TableRenderer::error` (`output.rs:142`).
3. The irreversible marker lives in a **different crate**: `crates/plan/src/preview.rs` `Display` impl
   — the ` (!)` at line 99 and the `(!) irreversible: N node(s)` summary at lines 106-114. `Display`
   can't take config, so either add a separate render fn that takes the color flag, or colorize the
   marker in the renderer when printing the preview. Also color the error path in `render_run_error`
   (`cmd/lib.rs:803`, stderr).

## Key files

- `crates/exec/src/output.rs:{26,67,109,142,351-393}`, `crates/plan/src/preview.rs:{92-116}`,
  `crates/qfs/src/cmd/lib.rs:{222-229,690-797,803}`.

## Considerations

- Respect `NO_COLOR` precedence (env disables even on a TTY); never emit ANSI to a pipe/file or under
  `--json`. Bump the patch in `crates/qfs/Cargo.toml`. Keep escapes minimal and centralized.
