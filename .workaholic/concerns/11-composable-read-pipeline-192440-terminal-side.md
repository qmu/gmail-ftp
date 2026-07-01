---
origin_pr: 11
origin_pr_url: https://github.com/qmu/qfs/pull/11
origin_branch: work-20260629-110121
origin_commit: 3c6f995
created_at: 2026-07-02T01:21:00+09:00
severity: moderate
status: active
resolved_by_pr:
resolved_by_commit:
---

# Composable read pipeline (192440) terminal-side follow-ups

## Description

The composable `array_agg(struct)` read pipeline landed ([b5a4eec]), but the terminal `INSERT … FROM` does not yet materialise rows commit-side (a pre-existing runtime/interpreter gap), and the live Gmail send behind the irreversible gate needs the owner's Google account. So the Drive-to-Gmail attach-and-send payoff is demonstrable on the read leg but not yet end-to-end.

## How to Fix

Build commit-side row materialisation for `INSERT … FROM` a computed pipeline, then wire and live-verify the gated Gmail send; both are captured as scoped follow-ups in the roadmap.
