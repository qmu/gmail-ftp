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

# project.db migration mismatch / store flakiness (203120)

## Description

A pre-existing `~/.config/qfs/project.db` migration mismatch surfaced intermittently during live verification; the migration guide and live-verification tickets (see [30e5ca7], [cd41ddb]) each worked around it with a fresh `XDG_CONFIG_HOME`. The forward-heal for Project v2's `1be5979f` in-place edit ([9b46d6c]) fixed one known checksum, but the underlying issue was never confirmed-ticketed and remains open. The CONNECT epic's migration 8 raises the stakes since project.db is now the single source of truth for path bindings.

## How to Fix

File/confirm a ticket for 203120, reproduce deterministically, and audit the migration runner's isolation. Every future in-place-edit-that-ships must add its own `SUPERSEDED_BODIES` entry; consider consolidating the runner given qfs is not a long-lived server.
