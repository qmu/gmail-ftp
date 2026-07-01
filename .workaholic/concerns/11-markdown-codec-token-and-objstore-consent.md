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

# Markdown codec token and objstore consent-gate reconciliation

## Description

The markdown codec now resolves as `md` ([69fd0c8]); separately, the `CLOUD_DRIVERS` consent set lists `objstore` while the driver ids are `s3`/`r2`, so the bind gate is effectively off for s3/r2 ([cf08355]) — worth reconciling so the consent gate matches the real driver ids.

## How to Fix

Align the `CLOUD_DRIVERS` consent set with the actual `s3`/`r2` driver ids so the bind gate governs object-storage reads consistently.
