---
created_at: 2026-06-30T01:01:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 1h
commit_hash:
category: Changed
depends_on: [20260630010090-plain-language-onboarding.md]
---

# Let the first step succeed: lead onboarding with a LOCAL command, not a cloud one

Roadmap "Onboarding & polish": the post-install "next steps" push the reader toward a **cloud**
command before they've done anything that works — a new user with no account hits a wall and leaves.
The first shown command should be a **local** command that returns real output.

## Current state (confirmed)

- `packages/qfs/install.sh:27-31` (`next_steps()` step 1, line 29) leads with
  `qfs describe /mail/drafts` then `INSERT INTO /mail/drafts …` — both `/mail` (cloud) paths.
- `README.md:112-130` ("Quickstart (the loop)", line 116) also opens with `qfs describe /mail/drafts`.
- By contrast `docs/guide/getting-started.md` is already correct — it opens with
  `qfs describe /local/...` and states the first sections "run offline with no credentials." Use it
  as the model.

## Plan

1. Reorder `install.sh` step 1 and the `README` Quickstart step 1 to lead with a working **local**
   command — e.g. `qfs run "/local/etc |> select name, size, is_dir |> limit 5"` or a `decode/encode`
   convert (both already in README at lines 120-124).
2. Move the `/mail/drafts` describe/insert example to a later "preview a write / connect a service"
   step. (`describe /mail/drafts` does work offline, but the point is the *first* command should
   return real rows.)

## Key files

- `packages/qfs/install.sh:27-31` (primary), `README.md:112-130`. Keep `getting-started.md` as-is.

## Considerations

- **Depends on `20260630010090`** (plain-language onboarding): both edit `install.sh next_steps()` and
  `README` — serialize to avoid clobbering. Bump the patch in `crates/qfs/Cargo.toml`.
