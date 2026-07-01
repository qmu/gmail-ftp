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

# /git @<ref> tree/blob reads and nested subtrees still limited

## Description

Time-travel now works for commits/refs/tags and for `@<ref>` tree and single-blob reads ([c5cfa89], [794d8f8], [8075c77]), but blob reads resolve flat-tree (E0) only — nested subtree paths remain out of scope. The docs claim only what runs.

## How to Fix

Extend blobfs dispatch to resolve nested subtree paths; keep the structured `invalid_path` fail-closed for genuinely missing paths.
