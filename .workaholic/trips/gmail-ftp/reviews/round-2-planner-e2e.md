# Round-2 Planner E2E Review — Credential-Free Smoke Suite (S1–S10)

Author: Planner
Status: complete
Phase/step: coding/review-and-testing
QA domain: E2E / external-interface (CLI execution from the terminal)
Reference plan: `/home/ec2-user/projects/gmail-ftp/.workaholic/trips/gmail-ftp/e2e-plan.md`

## Environment

- Toolchain: `export PATH="$HOME/sdk/go/bin:$PATH"` (`go1.24.4 linux/arm64`), `GOTOOLCHAIN=auto`.
- Build target: `go build -o gmail-ftp .` (binary is gitignored; created for testing, not committed).
- No live Gmail OAuth credential exists in this trip. All commands use throwaway
  paths `-creds /tmp/none.json -token /tmp/none.token.json` to guarantee the
  "unauthenticated" state without touching any real token.
- `~/.config/gmail-ftp/` does not exist (no prior runs); the audit log is absent/empty.

## Content

### Results table (S1–S10)

| ID | Command (actual) | Expected | Observed | Verdict |
|----|------------------|----------|----------|---------|
| S1 | `go build -o gmail-ftp .` | exit 0; executable produced | exit 0; 21 MB `gmail-ftp` produced; no warnings | PASS |
| S2 | `./gmail-ftp --help` ; `./gmail-ftp -h` | usage block on stderr listing `-creds`/`-token`/`-json`/`-no-log` and `auth`/`log`/`completion zsh` hints; mentions the interactive shell; no panic, no auth attempt | Both print the full usage block (all four flags + the three subcommand hints + "With no command, an interactive FTP-like shell is started"); exit 0; no panic | PASS |
| S3 | `printf 'quit\n' \| ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json` | banner+clean exit OR fail-fast clear unauthenticated error; no hang/panic | Build authenticates **eagerly** before the REPL: fails fast with a clear, human-readable message (`reading credentials "/tmp/none.json": ... no such file or directory` + a "Download an OAuth Desktop-app client ... or pass -creds" hint) on stderr; exit 1; no hang, no panic | PASS (fail-fast branch) |
| S4 | one-shot `... bogusverb` ; interactive `printf 'bogusverb\nquit\n' \| ...` | friendly unknown-command error (no stack trace); one-shot exits non-zero; record which layer rejects first | Both rejected by the **eager-auth layer** (same credentials error) before command dispatch; exit 1; no panic/stack trace. The unknown-command UX itself is **gated behind auth** and not observable credential-free | PASS (auth layer rejects first; recorded) |
| S5 | text `... ls /` ; json `-json ... ls /` | text: clear human-readable missing-credential error on stderr, exit non-zero, no panic; **json: `{"error":"…"}` envelope on stderr**, exit non-zero | Text: clear credentials error, exit 1, no panic — **PASS**. JSON: emits the **same plain-text error**, NOT the `{"error":"…"}` envelope; exit 1, no panic — **deviation** | PARTIAL — text PASS, JSON envelope FAIL |
| S6 | `printf 'lpwd\nlls\nlcd /tmp\nlpwd\nquit\n' \| ./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json` | locals print/list/change/print; succeed even with no token; if auth is required first, record as UX finding | Eager auth blocks the REPL before any local input is read; locals never run credential-free. **Verified consistent with gdrive-ftp** (same `auth.Client`-before-shell structure) — not a gmail-ftp regression | PASS (consistent with gdrive-ftp; plan's "locals pre-auth" assumption corrected — see Concern 2) |
| S7 | `--help`; README/help table for `put` | `put` documented as create-a-draft, **never sends**; no send-on-put surface | `--help` does not detail `put` (and the in-REPL `help` table is auth-gated). README states `put <local>`: "Create a **draft** ... **Never sends.**" and the header "It also **never sends**: `put` only creates a draft". No send-on-put path exists in the published surface | PASS (surface assertion via README; in-REPL help auth-gated) |
| S8 | one-shot `... send /tmp/whatever.eml` ; `... label x y` ; `... unlabel x y` | `send`/`label`/`unlabel` are not working v1 verbs; no network mutation; exit non-zero for one-shot; in no case may `send` send mail | All three exit 1 via the eager-auth wall **before any dispatch or network call** — so no send/label mutation can occur credential-free (core safety satisfied). The explicit "deferred to v1.1" runtime message is auth-gated and not observable here; README command table documents `send` and `label`/`unlabel` as "**Deferred to v1.1**" | PASS (no mutation possible; deferral documented; runtime deferral message unverifiable credential-free) |
| S9 | `... log` (text, pipe) ; `-json log` | reads local `audit.jsonl` with no auth; empty log → "no operations" (text) or `[]` (JSON); exit 0; no panic | JSON: emits `[]`, exit 0 — **correct**. Text (non-TTY pipe): emits **empty output** (no "No operations logged yet" message), exit 0, no panic. Branches before `auth.Client` as designed | PASS (with minor text-mode note — see Concern 3) |
| S10 | `./gmail-ftp completion zsh` ; `... __complete ls ''` | `#compdef gmail-ftp` zsh script on stdout, exit 0, no auth; `__complete` silent with no token, exit 0 | `completion zsh` prints the `#compdef gmail-ftp` script (sources `__complete` on Tab), exit 0, no auth. `__complete` with throwaway token prints nothing (bails before `auth.Client` on missing token), exit 0 | PASS |

**Pass tally:** 9 PASS, 1 PARTIAL (S5 — text PASS, JSON error-envelope FAIL). Zero panics across the entire suite.

### Auth-timing characterization (cross-cutting)

The build authenticates **eagerly**: `main` calls `auth.Client(ctx, creds, token)`
once before constructing the shell, so the REPL and one-shot command dispatch only
run after credentials load successfully. Consequences observed: S3/S4/S6/S7/S8's
in-REPL behaviors (banner, unknown-command UX, local commands, `help` table,
deferred-verb messages) are all **gated behind the auth check** and cannot be
exercised credential-free. This timing is **identical to gdrive-ftp's** (verified:
both place `auth.Client` before the shell), so it is a faithful mirror of the
reference tool, not a gmail-ftp-specific regression. The plan's S6 hypothesis that
"locals work pre-auth (like gdrive-ftp)" was an incorrect premise — gdrive-ftp's
locals are also inside the post-auth REPL.

## Overall E2E decision

**Approve with minor suggestions.**

The binary builds cleanly, the unauthenticated failure path is graceful and
human-readable with zero panics, the credential-free subcommands (`completion zsh`,
`__complete`, `log`) behave exactly as specified, and — most importantly for the
locked v1 scope — **no `send`/`label`/`unlabel` mutation is reachable** and the
`put`-is-draft-only / `send`-deferred safety promises are documented on the
published surface. The single substantive gap (S5b JSON error envelope) is a
scripting-contract deviation, not a safety or data-integrity issue, hence "minor
suggestions" rather than "request revision".

## Critical Review Policy — Concerns and proposals

**Concern 1 (primary) — JSON mode does not honor the error-envelope contract for pre-auth failures.**
The `-json` flag promises a machine-readable `{"error":"…"}` envelope, and the
plan names that envelope the automation author's scripting contract. But
credentials/auth failures (the single most common failure on a fresh host) exit
via `fatal(err)`, which writes plain text to stderr regardless of `-json`; only
errors raised *after* `auth.Client` succeeds (from `sh.Execute`) get
`shell.EncodeErrorJSON`. **Business impact:** an automation author who wraps
gmail-ftp and parses stderr as JSON will hit a parse error precisely on the most
frequent failure, eroding the "predictable scripting contract" value proposition
for the automation-author and sysadmin personas.
**Proposal (business outcome — a reliable JSON contract for unattended runs):**
route pre-auth/early `fatal` failures through the same JSON envelope when `-json`
is set (e.g. emit `{"error":"reading credentials \"…\": …"}` to stderr, keep
exit 1). This makes "every gmail-ftp error is parseable JSON under `-json`" an
unconditional promise an agent can depend on — the outcome that makes the tool
safe to embed in pipelines and AI-agent loops.

**Concern 2 (scope/UX) — every credential-free in-REPL affordance is hidden behind eager auth.**
Because auth runs before the shell, a brand-new user with no `credentials.json`
cannot reach `help`, `lpwd`/`lls`, or even read which verbs exist — they only see
the credentials error. This is faithful to gdrive-ftp, so it is not a defect, but
it does cap the "does it feel discoverable?" first-run experience for the
terminal-first power-user persona.
**Proposal (business outcome — a confident first run without credentials):**
keep eager auth as-is for all remote verbs (do not destabilize the proven model),
but let `help` (and only `help`) short-circuit before `auth.Client`, mirroring how
`completion`/`log`/`__complete` already branch pre-auth. A user can then discover
the command surface and the `put`-never-sends / `send`-deferred safety promises at
the prompt before investing in OAuth setup — strengthening trust at the exact
moment adoption decisions are made. Low risk; additive.

**Concern 3 (minor) — empty audit log prints nothing in piped text mode.**
`log` in a non-TTY pipe with an empty log emits zero bytes (the friendly
"No Gmail operations have been logged yet." line lives only on the interactive-TTY
branch). JSON mode correctly emits `[]`. Empty output is defensible for machine
consumption, but a human running `gmail-ftp log` inside a script/CI log sees
nothing and may suspect a failure.
**Proposal (business outcome — unambiguous audit feedback):** in the non-TTY text
branch, when there are zero entries, print the same one-line "No Gmail operations
have been logged yet." message (still exit 0). Operators auditing tool activity
then always get an explicit, trustworthy signal rather than silence.

## Deferred (credential-gated) scenarios — G1–G8

Out of scope for this automated step (no live OAuth token / disposable mailbox in
this trip). Documented in the plan §3 to be run verbatim once a credential exists,
against a **throwaway** Gmail account only:

- **G1** — terminal OAuth flow; least-privilege scope audit (`gmail.modify` + `gmail.compose`, no `mail.google.com`).
- **G2** — `ls /` lists labels (system first, then user); `-json` array with `kind:"label"`.
- **G3** — `cd INBOX` → `ls` lists messages; `pwd` = `/INBOX`; message is a leaf (not a `cd` target).
- **G4** — `get` downloads a message `.eml` and an attachment (atomic temp-rename; `downloaded` audit entry).
- **G5** — `find` (client-side subject substring) vs `search` (native Gmail query); neither mutates.
- **G6** — `put` creates a draft (behavioral proof of S7: addressable draft, `drafted` audit entry, nothing sent).
- **G7** — `rm` trashes exactly one message reversibly (TRASH label, never hard-delete; `trashed` audit entry).
- **G8** — `mkdir` creates a user label visible at root (`created label` audit entry).

Recommendation: run G6 (draft-only proof) and G7 (single-message reversible trash)
first when a credential lands — they are the behavioral confirmations of the v1
"nothing irreversible by accident" safety bar.

## Review Notes

- Tested against the committed source on branch `work-20260620-200140`; `gmail-ftp`
  binary built fresh for this run (gitignored, not committed).
- No source files were modified; no unit tests run; no code review performed
  (those are the Constructor's and Architect's domains).
- Defects are characterized, not fixed — remediation is the Constructor's call.
