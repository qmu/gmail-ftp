# E2E Test Plan v1

Author: Planner
Status: draft
Scope: gmail-ftp v1 (locked by plan.md → Amendment 1)
QA domain: E2E / external-interface (CLI execution from the terminal)

## Content

### 0. Purpose and boundaries

This plan defines terminal-runnable E2E scenarios that validate gmail-ftp from the
**outside** — the same way a persona invokes it — for the **locked v1 scope**:
`auth`, navigate (`ls`/`cd`/`pwd`), `find`/`search`, `get`, `rm` (trash),
`mkdir` (create label), `put` (create **draft only**), local `lcd`/`lls`/`lpwd`,
and audit logging. The irreversible `send` verb and `label`/`unlabel` membership
verbs are **deferred to v1.1** (Amendment 1); v1 must surface them as
deferred/stubbed, never wire them into live dispatch.

This is **not** unit testing (Constructor's domain) and **not** code review
(Architect's domain). It exercises the built binary's CLI contract.

The invocation experience mirrors gdrive-ftp exactly (verified against
`/home/ec2-user/projects/gdrive-ftp/README.md` and `main.go`):

- Build: `go build -o gmail-ftp .`
- No-arg launch → interactive shell with a banner and a `gmail:/>`-style prompt.
- One-shot: `gmail-ftp [flags] <verb> [args...]` runs one command and exits.
- Subcommands that need **no** auth: `completion zsh`, `log`, `__complete`,
  `auth` (runs the flow), plus `--help`/usage via `flag`.
- Flags: `-creds`, `-token`, `-json`, `-no-log` (same shape as gdrive-ftp).
- Config dir `~/.config/gmail-ftp/` holds `credentials.json`, `token.json`,
  `audit.jsonl`.

### 1. Environment / toolchain status (validated, read-only)

- **Go toolchain:** present and usable at `/home/ec2-user/sdk/go/bin/go`
  (`go version go1.24.4 linux/arm64`). **Not on the default `$PATH`** — every
  E2E command in this plan must export it first:
  `export PATH="$HOME/sdk/go/bin:$PATH"`.
- **Toolchain auto-switch:** gdrive-ftp's `go.mod` declares `go 1.25.8` and
  design-v2 targets `module gmail-ftp, Go 1.25.x`. `GOTOOLCHAIN=auto` is set and
  the `golang.org/toolchain@…go1.25.8` / `go1.25.11` toolchains are already in the
  module cache, so an offline build that requires go1.25.x will auto-switch
  cleanly. (If a build ever reports a toolchain-download need, that is an
  environment finding to report, not a product defect.)
- **Dependencies:** `google.golang.org/api@v0.284.0` (which ships **both**
  `drive/v3` and `gmail/v1`) plus `golang.org/x/oauth2`, `golang.org/x/term` are
  already in the local module cache (`/home/ec2-user/go/pkg/mod`). `proxy.golang.org`
  is reachable (HTTP 200), `GOPROXY=https://proxy.golang.org,direct`. So
  `go mod tidy`/`go build` succeed whether online or offline from cache.
- **No prebuilt binary** exists for either project; the Review-and-Testing step
  will build `gmail-ftp` fresh from the Constructor's source.
- **No live Gmail OAuth credential** is available in this trip. All
  credential-gated scenarios (§3) are therefore documented-but-deferred.

### 2. Credential-free smoke scenarios (EXECUTED in Review-and-Testing)

These need no Gmail account, no `credentials.json`, no `token.json`. They are the
scenarios the Planner will run in the next step (Review-and-Testing). Each is a
real terminal command sequence. Run all from the repo root
`/home/ec2-user/projects/gmail-ftp` with `export PATH="$HOME/sdk/go/bin:$PATH"`,
and use an empty/throwaway config dir to guarantee "unauthenticated" (e.g.
`-creds /tmp/none.json -token /tmp/none.token.json`) so no real token is touched.

#### S1 — Binary builds
- **Precondition:** Constructor's source is committed; toolchain on PATH.
- **Steps:** `go build -o gmail-ftp .`
- **Expected:** exit 0; a `gmail-ftp` executable is produced. (Sanity:
  `go vet ./...` is Constructor's bar, not re-run here; we only need a build.)
- **Persona:** all three — nothing works if it does not build.

#### S2 — Usage / help
- **Precondition:** binary built (S1).
- **Steps:** `./gmail-ftp --help` ; also `./gmail-ftp -h`.
- **Expected:** prints the usage block to stderr listing flags
  (`-creds`/`-token`/`-json`/`-no-log`) and the `auth`/`log`/`completion zsh`
  subcommand hints; mentions the interactive shell. Exit status per `flag`'s
  convention (non-zero for `-h` is acceptable as long as usage prints). No panic,
  no auth attempt.
- **Persona:** terminal-first power user discovering the tool; automation author
  scripting `--help` parsing.

#### S3 — Interactive shell starts and prompts
- **Precondition:** binary built; an empty config dir so no token exists.
- **Steps:** pipe an immediate `quit` into the interactive shell with throwaway
  paths, e.g. `printf 'quit\n' | ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json`.
- **Expected:** EITHER the connect banner ("Connected to Gmail…", `gmail:/>`
  prompt) appears and the shell exits cleanly on `quit` (if the build defers auth
  until first remote call), OR it fails fast with a clear unauthenticated error
  (see S5). Determine the build's actual auth-timing and assert the matching
  behavior. No hang, no panic.
- **Persona:** terminal-first power user — "does it feel like gdrive-ftp?"

#### S4 — Unknown-command handling
- **Precondition:** binary built.
- **Steps (interactive):** `printf 'bogusverb\nquit\n' | ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json`.
  **Steps (one-shot):** `./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json bogusverb`.
- **Expected:** a friendly "unknown command" style error (mirroring gdrive-ftp),
  not a panic/stack trace; interactive session stays alive and reaches `quit`;
  one-shot exits non-zero. Note: if the build reaches command dispatch only after
  auth, the unknown-command path may be gated behind S5's auth error — record
  which layer rejects first.
- **Persona:** all personas — graceful failure builds trust.

#### S5 — Graceful error when unauthenticated
- **Precondition:** no valid `credentials.json`/`token.json` (throwaway paths).
- **Steps:** `./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json ls /`.
- **Expected:** a clear, human-readable error about missing credentials / not
  authorized (e.g. cannot read credentials, or run `auth` first) on stderr, exit
  non-zero. **No panic, no nil-pointer, no raw Go error dump.** With `-json`
  (`./gmail-ftp -json -creds … -token … ls /`) the error is the `{"error":"…"}`
  envelope on stderr and still exits non-zero.
- **Persona:** automation author — predictable non-zero exit + JSON error
  envelope is the scripting contract; sysadmin on a fresh host.

#### S6 — Local commands work without any Gmail auth (`lcd`/`lls`/`lpwd`)
- **Precondition:** binary built; a known local directory with a couple of files.
- **Steps (interactive):**
  `printf 'lpwd\nlls\nlcd /tmp\nlpwd\nquit\n' | ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json`
- **Expected:** `lpwd` prints the current local working dir; `lls` lists local
  files; `lcd /tmp` changes it; the second `lpwd` reflects `/tmp`. These are pure
  local-FS helpers (copied verbatim from gdrive-ftp) and must succeed **even with
  no Gmail token**. If the build requires auth before the REPL accepts any input,
  record that as a UX finding (gdrive-ftp's locals do not need Drive auth).
- **Persona:** terminal-first power user staging files before/after transfers;
  automation author.

#### S7 — `put` only ever creates a draft (never sends)
- **Precondition:** binary built. This is a **contract/UX assertion** verifiable
  without live Gmail: we confirm the *surface* never offers a send-on-put path.
- **Steps:**
  1. `./gmail-ftp --help` and the interactive `help put` / `help` table
     (`printf 'help\nquit\n' | ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json`).
  2. Inspect the help text for `put`.
- **Expected:** `put` is documented as **"create a draft"** and explicitly states
  it **never sends**. No flag or syntax on `put` triggers a send. (Behavioral
  proof of the draft round-trip is credential-gated — see S/G6 — but the
  *guarantee that put cannot send* is assertable from the published command
  surface here.)
- **Persona:** all personas — the v1 safety promise ("no accidental irreversible
  actions"); sysadmin who must never fat-finger an outbound mail.

#### S8 — `send` reports deferred to v1.1
- **Precondition:** binary built. Per Amendment 1, `send` (and `label`/`unlabel`)
  are deferred; they must not be live v1 verbs.
- **Steps (one-shot):** `./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json send /tmp/whatever.eml`
  and interactive `printf 'send x\nhelp\nquit\n' | ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json`.
- **Expected:** `send` is **not** a working verb. Acceptable v1 behaviors (assert
  whichever the build chose): (a) `send` is absent from the help table and treated
  as an unknown command, or (b) `send` is present but responds with an explicit
  **"deferred to v1.1"** / "not available in v1" message and performs **no**
  network mutation. In **no** case may `send` attempt to send mail. Same check for
  `label`/`unlabel`. Exit non-zero for the one-shot.
- **Persona:** stakeholder/business — verifies the Lead's locked v1 scope is
  honored and the irreversible action is genuinely held back.

#### S9 — Audit-log subcommand reads locally without auth
- **Precondition:** binary built; empty/non-existent audit log.
- **Steps:** `./gmail-ftp -token /tmp/none.token.json log` and
  `./gmail-ftp -json log`.
- **Expected:** `log` reads only the local `audit.jsonl`, needs **no** Gmail auth
  (branches before `auth.Client`, like gdrive-ftp). With an empty log it prints a
  "no operations logged yet" message (text) or `[]` (JSON), exit 0. No panic.
- **Persona:** sysadmin/operator auditing what the tool did; automation author
  consuming JSON.

#### S10 — `completion zsh` emits a script without auth
- **Precondition:** binary built.
- **Steps:** `./gmail-ftp completion zsh`.
- **Expected:** prints a `#compdef gmail-ftp` zsh completion script to stdout,
  exit 0, no auth attempt. (`__complete` with no token must stay silent — it must
  never launch OAuth; spot-check `./gmail-ftp -token /tmp/none.token.json __complete ls ''`
  prints nothing and exits 0.)
- **Persona:** terminal-first power user wiring up Tab completion at the zsh prompt.

**Smoke-suite pass criterion:** all of S1–S10 behave as above with **zero panics**
and a coherent, gdrive-ftp-consistent invocation experience. S3/S4/S6 have a
documented branch depending on the build's auth-timing; the Planner will record
the observed behavior and flag any divergence from gdrive-ftp's "locals + help +
log + completion work pre-auth" UX as a finding for the Constructor.

### 3. Credential-gated scenarios (DEFERRED to credentialed manual runs)

These require a real OAuth "Desktop app" `credentials.json` and a completed
consent flow producing `~/.config/gmail-ftp/token.json`, plus a throwaway Gmail
test account with known labels/messages. **No such credential exists in this
trip**, so these are documented for a later credentialed run and are **out of
scope for the automated Review-and-Testing step.** They should be run against a
**disposable test mailbox**, never a personal inbox, because some mutate state
(reversibly).

#### G1 — Auth flow (terminal OAuth)
- **Precondition:** valid `credentials.json`; no cached token.
- **Steps:** `./gmail-ftp auth` → follow the printed consent URL (`c` to OSC-52
  copy / `o` to open / manual), approve, paste the `127.0.0.1?...code=` redirect
  URL back.
- **Expected:** "Authorized. Token cached at …/token.json"; token file written
  `0600` under a `0700` config dir; the requested scopes are exactly
  `gmail.modify` + `gmail.compose` (least-privilege, no `mail.google.com`, no
  hard-delete). Re-running `auth` reuses/refreshes silently.
- **Persona:** every first-time user; security stakeholder (scope audit).

#### G2 — `ls /` lists labels
- **Precondition:** authorized.
- **Steps:** `./gmail-ftp ls /` ; `./gmail-ftp -json ls /`.
- **Expected:** lists Gmail labels with system labels first
  (INBOX/SENT/DRAFT/…) then user labels; JSON is an array of entry objects with
  `kind:"label"`. This is the root (virtual-root analogue).
- **Persona:** all — the first navigation step.

#### G3 — `cd INBOX` then `ls` lists messages
- **Precondition:** authorized; INBOX has messages.
- **Steps:** `printf 'cd INBOX\nls\npwd\nquit\n' | ./gmail-ftp`.
- **Expected:** `cd INBOX` enters the label; `ls` lists **messages** (date +
  subject names, `from`/`unread` columns), capped at the default page (~50) with a
  "showing N of many" hint on large labels; `pwd` prints `/INBOX`. A message is a
  leaf — `cd <message>` is rejected (not a directory). Each row carries a
  `threadId` field but no thread is a `cd` target.
- **Persona:** terminal-first user triaging; sysadmin scanning an alert label.

#### G4 — `get` pulls a message `.eml` / an attachment
- **Precondition:** authorized; a known message, one with an attachment.
- **Steps:** `./gmail-ftp get "<message-name>" ./msg.eml` (or
  `get id:<msgID> ./msg.eml`); list a message's parts via
  `./gmail-ftp ls "<message-name>/"`; then `get id:<msgID>/<attID> ./file.bin`.
- **Expected:** the message downloads as RFC822 `.eml`; the attachment downloads
  as raw decoded bytes via atomic temp-rename; sizes are non-zero and plausible;
  a `downloaded` audit entry is appended.
- **Persona:** automation author fetching attachments; operator archiving a thread.

#### G5 — `find` / `search`
- **Precondition:** authorized; messages with a known subject substring.
- **Steps:** `./gmail-ftp find report` ; `./gmail-ftp search "from:alerts is:unread"`.
- **Expected:** `find` does a client-side subject substring match within scope and
  prints matches; `search` runs native Gmail query syntax server-side. `-json`
  yields an array of entry objects. Neither mutates anything.
- **Persona:** sysadmin locating an alert thread; power user.

#### G6 — `put` creates a draft (round-trip proof of S7)
- **Precondition:** authorized; a local RFC822 `.eml` (or body file + recipient).
- **Steps:** `./gmail-ftp put ./draft.eml` → note returned draft id; verify it
  appears under the DRAFT label (`ls /` → `cd DRAFT` → `ls`); confirm **nothing
  was sent** (no new SENT entry).
- **Expected:** a draft is created and addressable; a `drafted` audit entry is
  written; **no send occurs**. This is the behavioral confirmation of the
  credential-free S7 contract assertion.
- **Persona:** all — the safe-staging promise proven against live Gmail.

#### G7 — `rm` trashes a single message, reversibly
- **Precondition:** authorized; a disposable message whose loss is acceptable.
- **Steps:** `./gmail-ftp rm "<message-name>"` (or `rm id:<msgID>`); confirm it
  left its label and gained TRASH (`cd TRASH` → `ls`); restore from Gmail UI/Trash
  to prove reversibility.
- **Expected:** exactly **one message** is trashed (the TRASH label applied, never
  a hard-delete, never a whole thread); a `trashed` audit entry is written; the
  message is recoverable. A whole thread is trashed **only** via the explicit
  `rm id:thread:<id>` form, never implicitly.
- **Persona:** every user relying on the "nothing irreversible by accident" bar.

#### G8 — `mkdir` creates a label
- **Precondition:** authorized.
- **Steps:** `./gmail-ftp mkdir "ftp-test-label"`; `./gmail-ftp ls /` to confirm
  it appears among user labels; clean up via the Gmail UI.
- **Expected:** a new Gmail **user label** is created and shows up at root; a
  `created label` audit entry is written. (Per Amendment 1, `mkdir` = create
  label; message-level membership `label`/`unlabel` is deferred to v1.1.)
- **Persona:** operator organizing mail; power user mirroring a folder workflow.

### 4. Execution split (explicit)

- **Executed by the Planner in Review-and-Testing (next step), credential-free:**
  S1, S2, S3, S4, S5, S6, S7, S8, S9, S10. These validate the build, the CLI
  contract, graceful unauthenticated failure, the local commands, the
  audit-log/completion subcommands, and the two business-critical safety
  guarantees (`put` is draft-only; `send`/`label`/`unlabel` are deferred to v1.1)
  — all from the published command surface without any live Gmail access.
- **Deferred to a credentialed manual run (out of scope for this trip's
  automated testing):** G1–G8. They require a real OAuth token and a disposable
  test mailbox. This plan documents them so they are runnable verbatim the moment
  a credential exists; the mutating ones (G6/G7/G8) are reversible and must target
  a throwaway account.

## Review Notes

_(To be appended after the smoke suite runs in Review-and-Testing.)_
