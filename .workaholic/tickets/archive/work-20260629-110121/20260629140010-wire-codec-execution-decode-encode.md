---
created_at: 2026-06-29T14:00:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash: 0841112
category: Added
depends_on: [20260629140000-wire-local-single-file-content-read.md]
---

# T2 — Make `decode`/`encode` codec stages execute (kill the silent no-op)

Part of EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. Phase 1. **Subsumes the foundation
"codec no-op" binary bug.** Multi-day — see sub-task breakdown (each ≤4h, one commit).

## Overview

`… |> decode json |> encode yaml` is parsed and planned, then **silently dropped at lowering** — the
output is byte-identical with or without the codec stages. This is the marquee "convert a file's format
in one line" feature and the single most damaging doc lie (plausible wrong output, no error). The six
codecs are already implemented and pure; the gap is the plan/exec path.

## Ground truth (verified 2026-06-29)

- Parser builds `PipeOp::Decode(Codec)`/`PipeOp::Encode(Codec)` (`crates/parser/src/grammar.rs:853`).
- Evaluator builds `PlanSource::Codec { input, fmt }` (`crates/core/src/eval.rs:613`).
- **Dropped here:** `crates/pushdown/src/lower.rs:256-265` — `PipeOp::Decode(_) | PipeOp::Encode(_) … => Ok(input)`
  (pass-through). `LogicalPlan` has **no `Codec` variant** (`crates/pushdown/src/logical.rs:119-204`).
- Codecs ready: `crates/codec/src/lib.rs` — `builtin_codecs()` / `CodecRegistry::with_builtins()`,
  pure `decode(bytes)->RowBatch` / `encode(RowBatch)->bytes` over the `qfs-types` row model.

## Sub-tasks (each a ≤4h commit)

1. **Plan model** — add `LogicalPlan::Codec { input, fmt, dir }` to `crates/pushdown/src/logical.rs`;
   change `lower_op` (`lower.rs:256-265`) to emit it instead of `Ok(input)`. Keep partition totality.
2. **Physical/exec** — handle the codec node in `crates/pushdown/src/planner.rs` + the executor
   (`crates/exec/src/read.rs` / `crates/engine/`): pull the `content` Bytes column (from T1), call the
   registry codec (`decode` → RowBatch; `encode` → collapse rows to a `content` Bytes row).
3. **Registry wiring** — make the `CodecRegistry::with_builtins()` reachable on the read path; map
   `fmt` (`json,yaml,toml,csv,jsonl,markdown`) to the codec.
4. **Tests + golden** — `decode json |> encode yaml`, `… encode toml/csv/md`; round-trips; update
   pushdown golden tests; a real `/local/<file>.json |> decode json |> encode yaml` emits YAML.

## Key files

- `crates/pushdown/src/{logical.rs,lower.rs,planner.rs}`, `crates/exec/src/read.rs` (or `crates/engine/`),
  `crates/codec/`.

## Considerations

- Codecs are local-only (never pushed to a driver) — keep them an executor-side stage.
- Decide the encode output shape (single `content` Bytes row vs raw stdout bytes) and how `--format`
  renders it; the docs (index hero, files.md) must match whatever ships.
- After this lands, retire the foundation "codec no-op" flag and update index/concepts/files docs (Phase 5).
