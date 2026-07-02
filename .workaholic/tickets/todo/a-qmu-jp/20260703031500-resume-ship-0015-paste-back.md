---
created_at: 2026-07-03T03:15:00+09:00
author: a@qmu.jp
type: housekeeping
layer: [UX, Infrastructure]
effort:
category: Changed
depends_on: []
---

# RESUME: v0.0.14 shipped; passphrase fix committed on this branch; next = paste-back consent → ship 0.0.15

Context-window checkpoint for a fresh `/drive`. The owner is **mid-first-use onboarding** and is
blocked on the released binary; the fix is committed here but unshipped. **Do
`20260703030000-paste-back-browser-consent.md` next, then `/report` + `/ship` this branch as
v0.0.15** so the owner redoes setup on a fixed binary.

## Position (verified 2026-07-03 03:1x JST)

- Branch **`work-20260703-022500`**, HEAD `6be57c3`; 2 commits ahead of `main`
  (`9b04649` the /dev/tty passphrase fix, `6be57c3` its ticket archive). **Unpushed, no PR.**
- THREE untracked ticket files (this resume, `20260703030000` paste-back consent,
  `20260703040000` CREATE ACCOUNT surface) — commit them with the first paste-back commit, or
  on their own.
- `main` = `0945382` = **v0.0.14, released** (8 assets + release note attached,
  https://github.com/qmu/qfs/releases/tag/v0.0.14). Local main synced.
- This branch already carries the patch bump **0.0.15** (`crates/qfs/Cargo.toml` + lockfile).
- Whole workspace green at `9b04649`: `cargo test --workspace` exit 0, clippy `-D warnings`,
  fmt, `gen-docs --check`, `gen-skills --check` all clean.

## What just shipped / landed (context)

- **v0.0.14** (main): ADR 0008 complete — mount-bound accounts, per-layer verbs
  (init/app/account/connect/vault/host), `connection` namespace retired, migration v11, docs
  hard-break sweep. Story: `.workaholic/stories/work-20260702-012808.md`.
- **This branch (unshipped)**: the v0.0.14 first-run regression fix — the passphrase prompt now
  gates on the CONTROLLING terminal (stderr tty + `/dev/tty` opens), not stdin, so
  `cat credentials.json | qfs app add google` prompts on a terminal. rpassword already read
  `/dev/tty`; only the gate was wrong (`tty.rs::can_prompt_secret`,
  `connection.rs::resolve_store_passphrase`). Red/green proven against the released 0.0.14; PTY
  e2e test `piped_stdin_secret_entry_prompts_passphrase_on_dev_tty` (util-linux `script -qec`,
  stdin=pipe, passphrase fed via the pty) locks it.

## Owner state (the first user — drives the priority)

- Real `~/.config/qfs` was wiped of an agent-contamination incident and re-initialized BY THE
  OWNER: `qfs init a@qmu.jp` done with THEIR passphrase; nothing else configured yet.
- On released 0.0.14 they hit the piped-stdin passphrase error at `app add`. Workaround given:
  `read -rs QFS_PASSPHRASE && export QFS_PASSPHRASE`, then
  `cat ~/.config/gmail-ftp/credentials.json | qfs app add google`, then the gmail-ftp
  refresh-token stdin import for `a@qmu.jp`, then `qfs connect /mail --driver gmail --account
  a@qmu.jp`. That serves **/mail only** — the gmail-ftp token lacks the Drive scope, and one
  `google:<email>:refresh_token` slot exists per account, so importing the gdrive-ftp token
  would overwrite it.
- The owner explicitly wants **gmail-ftp's paste-back browser consent** (print URL, `c` = OSC 52
  copy, authorize in the LOCAL browser, paste the redirected `http://127.0.0.1?...` URL or bare
  `code=` back; state-verified; NO listener, NO ssh port-forward). qfs's current
  `qfs_google_auth::authorize` (`crates/google-auth/src/authorize.rs:67`) binds a real loopback
  listener and can never receive the redirect over plain SSH. The ticket
  `20260703030000-paste-back-browser-consent.md` specifies the port (reference implementation:
  `~/projects/gmail-ftp/internal/auth/auth.go`); its quality gate includes a live
  `/drive` read after the union-scope consent.

## Remaining work — do in order

1. **`20260703030000-paste-back-browser-consent.md`** — implement on THIS branch. Owner is
   present/reachable for the live consent verification (it needs their browser).
2. **`/report` + `/ship` as v0.0.15** (patch already bumped) — the release unblocks the owner's
   onboarding: reinstall via install.sh, then init-less continuation (their vault already
   exists), `app add` prompts fine, `account add google` paste-back consent gives the
   union-scope token, `/mail` + `/drive` both work.
3. **`20260703040000-create-account-language-surface.md`** — owner-requested: a CREATE ACCOUNT
   statement so account declaration is language-first like CONNECT (secret VALUES stay
   out-of-band; references only). Carries OPEN DESIGN DECISIONS — confirm them with the owner
   before implementing. Depends on #1 (the consent flow it complements).
4. Then: `20260630203090` /cf live (needs a CF API token pasted by the owner + plan recast onto
   the connect model), then close epic `20260630203000`.

## Owner Q&A already settled this session (do not re-litigate)

- Multiple Google accounts per operator: fully supported (N accounts = N mounts); the
  one-token-per-email slot is PER GOOGLE ACCOUNT and is fine once consent carries the scope
  union. The old gmail-ftp/gdrive-ftp narrow tokens cannot both serve one email — the
  paste-back consent (#1) dissolves this.
- CONNECT/DISCONNECT statement forms verified live in a scratch home (they desugar to
  `/sys/paths` effects; DISCONNECT gates as irreversible).

## Build-host + workflow notes (do not relearn)

- `cd packages/qfs`; export `TMPDIR=/home/ec2-user/projects/qfs/.tmp` and `CARGO_INCREMENTAL=0`
  for every cargo run; full workspace suite ~8-10 min — run in background. `command rm` (rm is
  trash-aliased); zsh `noclobber` → use `>|`.
- Commit ONLY via workaholic commit.sh (6 message fields, then explicit files); archive via
  archive.sh. Ticket frontmatter validators: `type` ∈ enhancement|bugfix|refactoring|
  housekeeping; `effort` ∈ 0.1h|0.25h|0.5h|1h|2h|4h|empty.
- e2e subprocess tests must inject a scratch `XDG_CONFIG_HOME` (never inherit the host config);
  the PTY harness pattern is in `crates/cmd/tests/e2e_cli.rs`.
- **Never run qfs setup verbs against the real `~/.config/qfs` from an agent** — that caused the
  contamination incident; use scratch homes, and leave the owner's config to the owner.
- **Tone (owner feedback, binding):** when the owner hits a bug I introduced, own it plainly and
  fix it — never frame it as a "valuable finding" or celebrate it. Respond in Japanese.
