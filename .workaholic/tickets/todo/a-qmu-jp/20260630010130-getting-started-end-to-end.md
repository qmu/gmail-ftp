---
created_at: 2026-06-30T01:02:10+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 4h
commit_hash:
category: Changed
depends_on: [20260630010120-connect-each-service-guide.md]
---

# "Get started" should be a practical, end-to-end on-ramp

Roadmap "Onboarding & polish": today "Get started" is a short first-queries page. It should grow into
the overall on-ramp that walks a new user all the way to the **full** feature set — local query,
convert a file, connect a service, query a database, join across sources, and preview/commit a
change — each step runnable, building on the last. (The conceptual "How qfs works" already moved out
of *Get started* into *Using qfs*, so the on-ramp can stay hands-on.)

## Current state (confirmed)

`docs/guide/getting-started.md` (title "Your first queries") already covers local query → convert
file (line 77) → SQLite (line 128, marked "(Optional)") → preview (151) → commit (188) → connect a
service (210). What it **lacks** per the roadmap:

- a *connect a service* step that actually **succeeds** (it currently shows error-then-narrative, no
  completed cloud query);
- a **database query** as a first-class numbered step (SQLite is "(Optional)" today);
- a **join across sources** step (absent).

## Plan

Restructure into a progressive numbered on-ramp, each step runnable and building on the last:
1. local query → 2. convert a file → 3. query a local DB (promote the "(Optional) SQLite" section to
a real step) → 4. connect a service (link the new per-service page from `20260630010120`) → 5. join
across sources (new — borrow a recipe from `docs/cookbook/cross-service.md`) → 6. preview → commit.
Consider renaming the page title "Your first queries" → "Get started" (update nav/sidebar labels in
`docs/.vitepress/config.mts:49` + ~50, `docs/index.md`, `README.md`).

## Key files

- `docs/guide/getting-started.md`, `docs/.vitepress/config.mts:{49,~50}`, `docs/index.md:{9-10,98}`,
  `README.md:110`, `docs/cookbook/cross-service.md` (join recipe source).

## Considerations

- **Depends on `20260630010120`** (connect-each-service guide) — step 4 links it.
- Honesty rule: every shown step must run against the binary as it is when this lands; if a cloud
  step still can't complete (Drive/GA reads, tickets `20260630010000`/`010010`), keep that step to a
  source that works (Gmail/SQL/local) rather than show a failing example. Hand-authored docs — no
  `gen-docs`. Bump the patch only if binary text changes.
