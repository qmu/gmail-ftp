# Round-3 Planner E2E Re-test — S5b JSON Error-Envelope Fix Verification

Author: Planner
Status: complete
Phase/step: coding/iteration-1 (re-test)
QA domain: E2E / external-interface (CLI execution from the terminal)
Scope: Re-verify the Constructor's S5b fix (pre-auth credential/auth failure must emit
the `{"error":…}` JSON envelope under `-json`); regression spot-check of previously
passing scenarios (S2, S9, send/label/unlabel deferral + no-panic).
Reference: prior verdict in `reviews/round-2-planner-e2e.md` (S5 PARTIAL — JSON envelope FAIL).

## Environment

- Toolchain: `export PATH="$HOME/sdk/go/bin:$PATH"` (`go1.24.4 linux/arm64`).
- Build: `go build -o gmail-ftp .` → exit 0, fresh 21 MB binary (gitignored, not committed).
- No live Gmail OAuth credential in this trip; unauthenticated state forced with throwaway
  paths `-creds /tmp/none.json -token /tmp/none.token.json`.
- Note on harness: the local zsh has `noclobber`; some `>` redirects to a pre-existing temp
  file printed a "file exists" shell warning. That is a redirect artifact of the test harness,
  not binary output — the binary's stderr, stdout, and exit codes were captured correctly in
  every case below.

## Fix under test

`main.go` now wraps every pre-auth fatal exit in `exitErr(jsonOut, err)`:

```go
func exitErr(jsonOut bool, err error) {
	if jsonOut {
		shell.EncodeErrorJSON(os.Stderr, err) // {"error": …} on stderr
		os.Exit(1)
	}
	fatal(err)                                // human-readable "gmail-ftp: …"
}
```

`auth.Client`, `gmailpkg.New`, the `auth` subcommand, the one-shot dispatch, and the
interactive entry all route their failure through `exitErr`, so a credentials/token failure
now honors the `-json` contract on every entry point.

## Results table

| ID | Command (actual) | Expected | Observed | Verdict |
|----|------------------|----------|----------|---------|
| S5b-1 | `./gmail-ftp -json -creds /tmp/none.json -token /tmp/none.token.json ls /` | `{"error":"…"}` on stderr, exit non-zero, empty stdout, no panic | stderr = `{"error":"reading credentials \"/tmp/none.json\": open /tmp/none.json: no such file or directory\nDownload an OAuth …, or pass -creds."}`; stdout empty; exit 1; `jq -e .error` succeeds; no panic | **PASS** |
| S5-text | `./gmail-ftp -creds /tmp/none.json -token /tmp/none.token.json ls /` | human-readable error on stderr, exit non-zero, NOT JSON, no panic | stderr = `gmail-ftp: reading credentials "/tmp/none.json": … or pass -creds.`; `jq` confirms it is **not** JSON; exit 1; no panic — no regression | **PASS** |
| S5b-2 | `./gmail-ftp -json … auth` | envelope (auth subcommand path) | valid `{"error":…}` envelope on stderr; stdout empty; exit 1 | **PASS** |
| S5b-3 | `printf 'quit\n' \| ./gmail-ftp -json … ` (interactive entry) | envelope before REPL | valid `{"error":…}` envelope on stderr; stdout empty; exit 1 | **PASS** |
| S2 | `./gmail-ftp --help` ; `./gmail-ftp -h` | full usage block on stderr (all 4 flags + auth/log/completion hints + interactive-shell note); exit 0; no panic | full usage block printed (`-creds`/`-json`/`-no-log`/`-token` + the three subcommand hints + "With no command, an interactive FTP-like shell is started"); exit 0; no panic | **PASS** (no regression) |
| S9-text | `./gmail-ftp log` (piped, empty log) | empty/clean, exit 0, no panic | stdout empty, stderr empty, exit 0; no panic (unchanged from round 2; the empty-text-line cosmetic note from round-2 Concern 3 persists but is unrelated to the S5b fix) | **PASS** (no regression) |
| S9-json | `./gmail-ftp -json log` | `[]`, exit 0 | stdout = `[]`, exit 0; no panic | **PASS** (no regression) |
| S8 | one-shot `send /tmp/x.eml`, `label a b`, `unlabel a b` — text and `-json` | no mutation reachable credential-free; exit non-zero; no panic; under `-json`, error is now an envelope too | all three hit the eager-auth wall before any dispatch/network call; text → `gmail-ftp:` plain error; `-json` → valid `{"error":…}` envelope; exit 1; **no panic in any of the six runs**. No send/label/unlabel mutation is reachable. (Runtime "deferred to v1.1" message remains auth-gated, as characterized in round 2 — unobservable credential-free, unchanged) | **PASS** (no regression; safety intact) |

**Tally:** 8/8 PASS. Zero panics across all runs.

## Updated S5 verdict

**S5 — PASS (upgraded from round-2 PARTIAL).** S5b is now **fully resolved**:

- `-json` mode emits a valid, parseable `{"error":"…"}` envelope on stderr for pre-auth
  credential/token failures, with exit 1 and empty stdout — verified with `jq -e .error`.
- The envelope is now consistent across **every** failure entry point (one-shot `ls`,
  `auth` subcommand, interactive entry, and the deferred `send`/`label`/`unlabel` verbs),
  not just post-auth `sh.Execute` errors. This satisfies round-2 Concern 1 exactly: "every
  gmail-ftp error is parseable JSON under `-json`" is now an unconditional promise.
- The default text mode is **unchanged** — still the human-readable `gmail-ftp: …` form,
  confirmed not-JSON. No regression to the human path.

The error message body retains an embedded newline (the credentials hint is on a second
line, inside the JSON string value as `\n`). This is valid JSON (the newline is properly
escaped) and `jq` parses it cleanly, so it does **not** affect the scripting contract; noted
only for completeness, not as a defect.

## Overall E2E decision

**Approve.** The single substantive gap from round 2 (S5b JSON error-envelope) is closed and
verified end-to-end, the human-readable text path is preserved with no regression, and the
spot-checked S-suite (S2 help, S9 log text+json, S8 send/label/unlabel deferral) is
unaffected with zero panics anywhere. No source modifications were made by the Planner; the
gitignored binary was rebuilt for testing only. No new defects found.

The credential-gated G1–G8 behavioral scenarios from round 2 remain deferred (no live OAuth
token in this trip) and are unchanged by this fix.

## Review Notes

- Re-tested against current committed source; `gmail-ftp` rebuilt fresh (gitignored).
- No source files modified; no unit tests run; no code review performed (Constructor/Architect domains).
- Round-2 cosmetic Concerns 2 (help auth-gated) and 3 (empty-text-log silence) are out of
  scope for this S5b re-test and remain as previously characterized — not regressions.
