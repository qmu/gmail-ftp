---
origin_pr: 11
origin_pr_url: https://github.com/qmu/qfs/pull/11
origin_branch: work-20260629-110121
origin_commit: 3c6f995
created_at: 2026-07-02T01:21:00+09:00
severity: low
status: active
resolved_by_pr:
resolved_by_commit:
---

# /local write materialization is narrow

## Description

Local writes persist, and a positional single-column payload now maps onto the blob ([0373cd2]), but a multi-column payload with no `content` column still errors — the user must name the blob column. Earlier in the branch one-shot `upsert into /local/<file>` reported COMMITTED without writing, which the fallback addressed for the unambiguous case.

## How to Fix

Keep the single-column fallback strict (intentional); document that multi-column local writes must name the blob column. Watch the commit.rs → effect.rs content-blob threading for other write paths.
