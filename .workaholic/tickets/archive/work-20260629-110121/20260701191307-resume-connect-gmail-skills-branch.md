---
created_at: 2026-07-01T19:13:07+09:00
author: a@qmu.jp
type: housekeeping
layer: [Infrastructure]
effort:
commit_hash: f171bd5
category: Changed
depends_on: []
---

# Resume: the CONNECT + Gmail + Skills branch is PR-ready — /report, then 203050 / 203090

**Session handoff (carry).** A long session filled the branch `work-20260629-110121` with a large,
green, PR-ready body of work. Nothing is half-implemented — every task below is committed. This
ticket orients a FRESH session: what shipped, what remains (all remaining work needs the owner
present), the one open blocker found along the way, and the immediate next action.

## Position

- **Branch:** `work-20260629-110121` (NOT merged). Version `packages/qfs/crates/qfs/Cargo.toml` =
  **`0.0.12`** (one bump for this whole PR — do NOT re-bump per commit).
- **Tree:** clean. All gates were green at handoff: `cargo build --workspace`,
  `cargo test -p qfs-test`, `clippy` on touched crates, `fmt`, `gen-docs --check`, `gen-skills
  --check`.
- **Immediate next action:** run **`/report`** to open/refresh the PR and the release note (the owner
  deferred it once; offer it first). Only after that (or if the owner wants it first) tackle the two
  remaining tickets below — both need the owner.

## What shipped on this branch (this session)

- **CONNECT / defined-paths epic (EPIC 20260701100000) — COMPLETE + closed.** `CONNECT`/`DISCONNECT`
  grammar + `path_binding` DB (migration v8) [`fdec633`]; canonical-id CALL routing for multi-segment
  mounts [`1dfc20e`]; registration redesign — nothing third-party pre-mounted, drivers mount from
  bindings, planning+describe registries only (read/apply are DriverId-keyed) [`e636b4b`]; epic +
  design keystone closed [`aecd301`/`4e56c3c`].
- **Cloud-read runtime panic fix** [`613c1f5`] — the "runtime within a runtime" `block_on` bug for
  all five cloud read facets (gmail/gdrive/ga/github/slack), the prerequisite for live reads.
- **Gmail cookbook** — renamed Mail→Gmail with a full Setup section, comprehensive recipes, and
  **verbatim (non-normalized) labels** — `/mail/inbox` works because Gmail's `label:` search is
  case-insensitive; qfs does NOT rewrite case [`ea9dd36`/`3a1a3ed`/`8299491`]. Table left-border in
  the renderer [`c3d801f`].
- **203030 — live Gmail + Drive verification (token import) — DONE** [`30e5ca7`]. Real `/mail/inbox`
  messages and `/drive/my` files read through the binary.
- **203040 — gmail-ftp/gdrive-ftp → qfs migration guide — DONE** [`cd41ddb`], `id:`-selector fix
  [`8aff39c`].
- **Cookbook articles → Claude Code Agent Skills — DONE** [`919cb15`]. `cargo xtask gen-skills
  [--check]` generates `plugins/qfs/skills/qfs-<topic>/SKILL.md` from each `docs/cookbook/*.md`
  (single source, anti-drift like gen-docs); 7 skills registered + loading; new verified-true ratchet
  `crates/test/tests/cookbook_skills.rs`.

## Remaining todo (ALL need the owner present — not `/drive`-able autonomously)

- **`20260630203050-qfs-as-claude-plugin-mcp.md`** — package qfs as a Claude plugin / MCP. Edits the
  host `~/.claude/settings.json`, which `workaholic:system-safety` blocks an agent from doing → the
  owner must run/approve those host edits.
- **`20260630203090-cf-live-d1-kv-queue.md`** — live Cloudflare D1/KV/Queue verification. Needs a real
  Cloudflare account + a pasted API token.
- **`20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`** (epic) — closes once 203050 + 203090 land.

## Open blocker discovered during verification (NOT yet ticketed by this session)

- **`20260630203120` project.db migration mismatch / store flakiness.** During the live Google
  verification the fresh-`XDG_CONFIG_HOME` store intermittently lost its `path_binding` rows +
  identity/consent between separate `qfs` invocations (a re-run showed "no defined paths" / "not
  authenticated"). **Workaround used:** do setup + read in ONE shell invocation against a fresh
  `XDG_CONFIG_HOME`. This is a real bug worth its own fix — confirm `20260630203120` covers it, or
  file it.

## Gotchas / recipes for the continuing agent

- **`id:` is NOT a pipe-SQL statement source.** `id:<msg>` / `id:thread:<id>` parse only as CLI
  `qfs describe` addressing; in a statement use the path form `/mail/<label>/<msg-id>` or a
  `where thread_id == '<id>'` filter. (Both the cookbook and the migration guide were corrected.)
- **`commit.sh --category` drops the FIRST file arg.** Stage yourself then use `--skip-staging` with
  NO trailing files. See memory `[[commit-sh-explicit-files-gotcha]]`.
- **Skills are generated, never hand-edited.** Edit `docs/cookbook/*.md` (the `skill_*` frontmatter +
  body) and re-run `cargo xtask gen-skills`; `--check` is the anti-drift gate.
- **Live Google re-verify recipe** (host-local, opt-in, owner's own tokens): fresh
  `HOME`/`XDG_CONFIG_HOME` under `.tmp/`, export `QFS_PASSPHRASE` + `QFS_GOOGLE_ACCOUNT=a@qmu.jp` +
  `QFS_GOOGLE_CLIENT_ID/SECRET` (from `~/.config/{gmail,gdrive}-ftp/credentials.json`), then in ONE
  invocation: `identity signup` → pipe the FTP `token.json` refresh token to
  `qfs connection add google 'a%40qmu.jp'` → `qfs connection add gmail default` (records consent) →
  `qfs connect /mail --driver gmail` → read. The two FTP tokens have SPLIT scopes (gmail vs drive),
  so one import proves one service; a single combined-scope consent gives both.
- **Disk was tight earlier** — freed by removing the warm `packages/qfs/target` (26G). `.tmp/` is
  gitignored scratch; `/tmp` is tmpfs. See memory `[[build-host-tmpfs-and-rm-trash]]`.
