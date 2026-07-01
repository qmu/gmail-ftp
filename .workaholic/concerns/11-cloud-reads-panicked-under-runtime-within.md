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

# Cloud reads panicked under runtime-within-runtime blocking

## Description

Every cloud read facet's client drives the shared reqwest transport via its own `block_on`; called from inside the async read executor (itself a tokio worker) this panics with "Cannot start a runtime from within a runtime" (see [613c1f5] and [cf08355]). Only objstore was guarded, so gmail/gdrive/ga/github/slack live reads crashed the process; the hermetic mock-client path never exercised it. Fixed on this branch, but the class is easy to reintroduce.

## How to Fix

Run any blocking transport call on a dedicated OS thread (`std::thread::scope`) with no tokio context, reducing a panic to a structured secret-free error. Apply the same treatment to every future blocking-transport integration.
