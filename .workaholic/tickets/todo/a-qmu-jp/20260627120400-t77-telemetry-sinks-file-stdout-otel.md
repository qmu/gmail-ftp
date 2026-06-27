---
created_at: 2026-06-27T12:04:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260626101100-t53-sys-driver-admin-views.md, 20260627120300-t76-hash-chained-audit-emission.md]
---

# t77 — Externalized telemetry: file / stdout / OTel sinks (M3)

## Overview

Implements the **M3** telemetry surface of roadmap **decision V** / §4.6: qfs **emits** three signals —
**audit** (the chained stream from t76), **metrics** (counters/histograms), **traces** (per-plan
`describe`→pushdown→combine→`commit` spans) — to one of **three prepared output sinks: `file`
(default), `stdout`, or `OTel` (recommended)**, and nothing else. Everything downstream (Prometheus via
the collector, Grafana, Datadog, a SIEM, qfs Cloud's dashboards) consumes one of those. There is **no
native Prometheus scrape endpoint** (reached via the OTel collector). qfs emits, does not store; the
sink default is `file` (explicit override on servers to `stdout`/`OTel`). Adds `/sys/metrics` to the
SysDriver (t53). This is the "self-host monitoring" unlock.

## Exact seams

- A **telemetry-emit seam** in the engine — instrument `preview`/`commit`/pushdown to produce metric
  samples and trace spans; reuse t76's audit emission as the audit signal.
- **Sink implementations** — `file` (line-structured, point at a rotating path locally), `stdout`
  (12-factor; the platform captures), `OTel`/OTLP exporter (traces + metrics + logs). Sink chosen by
  explicit config; `file` is the default.
- **Metrics** — counters/histograms: preview/commit counts, per-driver call latency, pushdown ratio,
  rate-limit hits, errors.
- **Traces** — spans around `describe` → pushdown-per-source → local combine → `commit`.
- `/sys/metrics` live view — add to the SysDriver (extends t53).

## Implementation steps

Each slice leaves the tree green.

1. **Emit seam + signal types.** Define metric/trace signal types; instrument the engine.
2. **Sinks.** `file` (default) + `stdout` + `OTel`/OTLP exporter; config + explicit override.
3. **Metrics.** Wire the counters/histograms listed above.
4. **Traces.** Per-plan spans across the pushdown/combine/commit stages.
5. **`/sys/metrics`.** Live-view node in the SysDriver (t53); document `file`-default / `OTel`-recommended.

## Key files

- The engine instrumentation seam.
- Sink + exporter modules (`file`/`stdout`/OTLP).
- SysDriver `/sys/metrics` (extends [[t53 — /sys driver + admin views]]).
- `crates/qfs/Cargo.toml` patch bump.

## Considerations

- **Emit, don't store** (decision V). Downstream tools consume; retention is theirs. No native
  Prometheus endpoint — Prometheus reads from the OTel collector.
- **Default sink is `file`, explicit override on servers** to `stdout`/`OTel` (a stateless Worker/Lambda
  has no persistent disk — §4.6 / decision F).
- **Metadata only.** Telemetry carries shape (verbs/paths/counts/latencies), never secrets or rows.
- **Depends on** [[t76 — Hash-chained audit event emission]] (audit signal) and [[t53 — /sys driver + admin views]] (`/sys/metrics`).
- **Versioning.** Own PR + patch bump + `v0.0.x` tag (CLAUDE.md).
