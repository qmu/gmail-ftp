---
created_at: 2026-07-01T21:50:00+09:00
author: a@qmu.jp
type: housekeeping
layer: [Infrastructure]
effort:
commit_hash:
category: Changed
depends_on: []
---

# Resume: branch is green + PR-ready — run /report, then 192440 (focused) / 203090 (owner token)

**Session handoff (carry).** A long session filled branch `work-20260629-110121` with a large, green,
PR-ready body of work — 11 commits ahead of the FTP-parity ticket drop. Nothing is half-implemented;
the tree is clean. This ticket orients a FRESH session: position, the immediate next action, what
shipped, what remains, one open loose end, and host gotchas.

## Position

- **Branch:** `work-20260629-110121` (NOT merged). **HEAD `d97a3bd`.** Version
  `packages/qfs/crates/qfs/Cargo.toml` = **`0.0.12`** — one bump for this whole PR; do NOT re-bump per
  commit.
- **Tree:** clean. All gates were green at handoff: `cargo build --workspace`, `cargo test -p
  qfs-test`, `clippy --workspace --all-targets -D warnings`, `fmt --all --check`, `gen-docs --check`,
  `gen-skills --check`, and the `cookbook_skills` parse ratchet.
- **Immediate next action:** run **`/report`** to open/refresh the PR + release note over the 11
  commits. The owner has **declined/deferred `/report` several times** this session — offer it first,
  but don't force it; if declined again, proceed to the remaining tickets below.

## What shipped this session (branch commits `fb0e2ba`..`d97a3bd`)

- **203050** [`fb0e2ba`] — qfs packaged as a Claude Code plugin (marketplace + `/plugin install
  qfs@qfs`); install docs later moved into `docs/guide/installation.md` by the cookbook reorg.
- **192443** [`d2c5e89`] — corrected the stale `files.md` Drive-reads warning.
- **192439** [`596a3ac`] — **Array/Struct/Bytes literals** (`[ ]` / `{ }` / `X'..'`) in the closed-core
  grammar (lexer tokens, `Literal::Array/Struct/Bytes`, typeck, eval lowering) + Gmail draft
  `attachments` column, proven by a parse-checked cookbook recipe.
- **192441** [`a491cf7`] — **Gmail attachment byte-read**: `GmailClient::get_attachment` (base64url
  decode in `mime.rs`) + the `/mail/<label>/<msg>/<att>` read arm.
- **192442** [`8eeb479`] — **mkdir parity**: `INSERT INTO /mail/labels` (new `MailPath::Labels` +
  `create_label`) and the Drive folder metadata-only `files.create` branch.
- **192440 reshaped** [`6d7c93d`] — see below; NOT implemented, now a scoped spec.
- **Cookbook per-driver reorg** [`e0a8687`, `d0c2d1d`, `d97a3bd`] — one cookbook per service (new
  **Google Drive**, **git**, **GitHub**, **Slack**; **Files & object storage** = local + s3/r2);
  deleted the "Replace gmail-ftp/gdrive-ftp" migration guide (plugin-install content rehomed to
  `installation.md`); marketplace skills updated (dropped `qfs-code`, added
  `qfs-gdrive/git/github/slack`); mount-path note → `::: info` box; **🚧** on section headings whose
  feature has a documented not-yet-wired part.

## Remaining todo (priority order)

1. **`20260701192440-cross-service-drive-to-gmail-attach-and-send.md`** — the dogfooding payoff
   (Drive→Gmail attach-and-send). **Reshaped into a detailed spec this session.** Owner-confirmed
   design: the composable `ARRAY_AGG(STRUCT(...))` pipe, NOT a monolithic `pack()`. **Key finding
   captured in the ticket:** qfs's read path has **no general per-row scalar-expression executor** for
   projections/aggregates (`WHERE` runs as a lowered `Predicate` via `engine::eval_predicate`;
   `SELECT`/`EXTEND` are by-name only; aggregates are the closed `pushdown::Aggregator` set). So this
   is a **~1–2 day language feature** (build that executor + generalize struct/array literals to
   `Expr::Array/Struct` + add single-column `ARRAY_AGG` + `INSERT ... FROM` folding), not a small
   aggregate add. Do it in a **focused session**; the ticket has the ordered plan + file:line pointers.
2. **`20260630203090-cf-live-d1-kv-queue.md`** — `/cf` live D1/KV/Queue read+commit. **Needs the
   owner present**: paste a real Cloudflare API token + account id (and a D1 database id). Hermetic
   code (live `CfRegistry` via API enumeration like `sql.rs`, facet registration gated by
   `cloud_bind_allowed`, `MockCfBackend` tests) is buildable first; the live read is the acceptance
   proof.
3. **`20260630203000-epic-replace-gmail-ftp-gdrive-ftp.md`** (epic) — closes once 192440 + 203090 land
   (203050 + the four other FTP-parity sub-tickets already shipped this session).

## Open loose end (verify or file)

- **`20260630203120` project.db migration mismatch / store flakiness** — flagged in the prior (now
  archived) resume ticket as a real bug: a fresh-`XDG_CONFIG_HOME` store intermittently lost
  `path_binding` rows + identity/consent between separate `qfs` invocations. It is **not confirmed
  ticketed.** Confirm `20260630203120` exists/covers it, or file it.

## Gotchas / recipes for the continuing agent

- **Build host (memory `[[build-host-tmpfs-and-rm-trash]]`):** `/tmp` is a small tmpfs and `rm` is
  trash-aliased. For every cargo run: `source ~/.cargo/env`, `export
  TMPDIR=/home/ec2-user/.cache/qfs-tmp CARGO_INCREMENTAL=0 PATH="$HOME/.cargo/bin:$PATH"`; use
  `command rm` to actually delete. The Cargo workspace is under `packages/qfs/`.
- **Commits (memory `[[workaholic-commit-convention]]` + `[[commit-sh-explicit-files-gotcha]]`):**
  commit only via `commit.sh`; it stages tracked changes (`git add -u`) — for a commit that includes
  NEW files, `git add -A` yourself then pass `--skip-staging` with NO trailing files. `--category`
  before the 6 positional message fields.
- **Anti-drift:** never hand-edit `docs/{language,drivers,server}.md` (regenerate via `gen-docs`) or
  `plugins/qfs/skills/*/SKILL.md` (edit the `docs/cookbook/*.md` article + run `gen-skills`). Every
  fenced ` ```qfs ` recipe must PARSE — `crates/test/tests/cookbook_skills.rs` is the ratchet. A new
  cookbook article also needs a `marketplace.json` `skills[]` entry.
- **Experimental posture (memory `[[experimental-no-backward-compat]]`):** hard breaks are fine; no
  compat shims. 192440's plan retires `Literal::Array/Struct` in favour of `Expr::Array/Struct`.
- **Version:** already `0.0.12` for this PR — bump the patch only when starting the NEXT PR, not per
  commit here.
