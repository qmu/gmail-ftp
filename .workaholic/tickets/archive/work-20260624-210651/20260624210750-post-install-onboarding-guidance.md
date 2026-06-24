---
created_at: 2026-06-24T21:07:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Infrastructure]
effort:
commit_hash:
category:
depends_on: []
---

# Post-install onboarding: tell the user how to test, authenticate, update, and where the docs are

## Overview

After `install.sh` finishes, the user "cannot feel it installed" — the script only prints
`installed qfs to <dir>`, a PATH note, and the `--version` line, then exits. A first-time user is
left with a binary and no idea what to do next: how to confirm it works, how to connect a service
(authenticate), how to update later, or where to read more.

Add a concise **"Next steps"** block printed at the end of a successful install that guides the user
through: (1) a safe first command to confirm it works, (2) how to authenticate a service, (3) how
to update the binary, and (4) a link to the docs on GitHub. Keep it short, copy-pasteable, and
honest (the first command must work offline with no credentials).

This is a delivery/UX gap in the installed artifact (運用 operation + 設計 design): the install path
exists and works, but the moment-of-first-success is missing.

## Key files

- `packages/qfs/install.sh` — the install script; the post-install output is lines ~100–105
  (`installed qfs to …`, the PATH note, `qfs --version`). The new block goes right after a verified
  install, before the script exits.
- `README.md` (repo root) — "Install" / "Quickstart" sections; keep the terminal block and the
  README in agreement (the README already shows the describe→preview→commit loop and `account add`).
- `docs/guide/getting-started.md`, `docs/guide/accounts.md`, `docs/cookbook/` — the canonical
  onboarding docs to link to (served on GitHub).
- CLI surface to reference (already implemented): `qfs --version`, `qfs describe <path>`,
  `qfs run '<stmt>'` (PREVIEW by default), `qfs account add/list <service> <name>`, `qfs skill`.

## Related history

- The install path itself was built and verified end-to-end in branch `work-20260622-230954`
  (`install.sh` + the release pipeline; `v0.0.1`/`v0.0.2` published). The script verifies sha256,
  installs, and runs `--version` — but stops short of onboarding. No prior ticket covers post-install
  guidance (this is the first).

## Implementation steps

1. In `packages/qfs/install.sh`, after the `--version` check succeeds, print a short **Next steps**
   block to stderr (consistent with the existing `say` helper), covering:
   - **Test it works (no credentials needed):** e.g.
     `qfs describe /mail/drafts` and a PREVIEW run such as
     `qfs run "INSERT INTO /mail/drafts VALUES ('a@b.com','Hi','Body')"` — note that `qfs run`
     previews by default and changes nothing until `--commit`.
   - **Authenticate a service (only needed to commit against a live service):**
     `qfs account add <service> <name>` (e.g. `mail`, `s3`, `github`) and `qfs account list`. State
     that the credential is never printed back.
   - **Update qfs:** re-run the install one-liner (it always fetches the latest release), and how to
     pin a version with `QFS_VERSION=vX.Y.Z`.
   - **Docs:** a link to the GitHub docs — the repo README and getting-started:
     `https://github.com/qmu/qfs#readme` and
     `https://github.com/qmu/qfs/blob/main/docs/guide/getting-started.md` (plus the cookbook).
   - **Agents:** one line that `qfs skill` prints the AI operating procedure.
2. Keep the block compact (≈6–10 lines), copy-pasteable, and gated on a *successful* install (don't
   print it if the binary failed to run).
3. Make sure every command shown is real and works offline where claimed (verify against the binary:
   `describe` and `run` PREVIEW are pure; `account add` is the auth entry point). Re-use the exact
   wording from `docs/guide/getting-started.md` so the script and docs don't drift.
4. Optionally mirror a trimmed "After installing" snippet near the top of `README.md`'s Install
   section so the guidance is discoverable without running the script.

## Considerations

- **Honesty:** the "test it works" command must succeed with no credentials (use
  `describe`/`run` PREVIEW). Do not show a command that would error for a fresh user.
- **PATH:** when the install dir isn't on `PATH`, the existing note already fires; the Next-steps
  examples should still read clearly (consider showing `qfs` and mentioning the PATH caveat once).
- **No secrets, ever** (least privilege / 運用): the auth guidance points to `qfs account add`; never
  suggest putting tokens on the command line or in a file.
- **Link target:** docs are currently viewed on GitHub (the VitePress site is Docker-dev only). Link
  to the repo's README/`docs/` on `github.com/qmu/qfs`. A *separate* future enhancement could
  publish the VitePress site to GitHub Pages and point here instead — note it, don't do it here.
- **Keep script ↔ docs in sync:** the same describe→preview→commit + account-add story already lives
  in `docs/guide/getting-started.md`; reference/echo it rather than inventing new phrasing.
- **Scope:** terminal output + a small README touch only. No CLI behavior change.
