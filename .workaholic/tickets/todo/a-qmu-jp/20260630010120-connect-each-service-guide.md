---
created_at: 2026-06-30T01:02:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 2h
commit_hash:
category: Added
depends_on: []
---

# A "connect each service" guide — per-source setup steps

Roadmap "Onboarding & polish": each source needs slightly different setup (Gmail/Drive use Google
sign-in; GitHub/Slack use tokens; S3/R2 use keys; SQL and git just point at a location). That
deserves its own short "Get started" page with the exact steps per source, linked from everywhere —
instead of one generic connections page.

## Current state (confirmed)

`docs/guide/connections.md` documents the *mechanism* generically (env var vs encrypted store,
storing/listing/removing/rotating) but gives **no per-service recipe** — no Gmail/Drive Google
sign-in flow, no GitHub/Slack token creation, no S3/R2 key steps. `getting-started.md:218-219` shows
only one Gmail error-then-`qfs identity signup` snippet.

## Plan

1. Create a new page `docs/guide/connect.md` (or `connecting-services.md`) with one short section per
   source:
   - **Gmail / Drive** → Google sign-in via `qfs identity signup <email>` + OAuth consent (ref
     `crates/qfs/src/google.rs`, `crates/google-auth/`).
   - **GitHub / Slack** → create a PAT / bot token, pipe it to `qfs connection add` via stdin.
   - **S3 / R2** → access key id + secret.
   - **SQL / git** → the `QFS_SQL_*` / `QFS_GIT_*` locators (today's mechanism; align with the
     forthcoming `CREATE CONNECTION` model, epic `20260630004100`, when it lands).
2. Register the page in `docs/.vitepress/config.mts` — nav (lines 27-40) and sidebar "Using qfs"
   group (~line 72, beside `connections.md`).
3. Link it from `getting-started.md`, `README.md:53`, and `connections.md`. Connections.md becomes
   the *reference*; the new page is the *do-this-per-service* how-to.

## Key files

- New `docs/guide/connect.md`; `docs/guide/connections.md`, `docs/.vitepress/config.mts:{27-40,44-90}`,
  `docs/guide/getting-started.md`, `README.md:53`.

## Considerations

- Honesty rule: document only what actually runs today (the env-var locators for SQL/git, the real
  `qfs connection add` flows). Hand-authored docs — no `gen-docs`. Bump the patch only if binary text
  changes (likely not). Get-started e2e (`20260630010130`) links this page, so land this first.
