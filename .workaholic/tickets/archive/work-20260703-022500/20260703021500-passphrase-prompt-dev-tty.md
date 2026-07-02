---
created_at: 2026-07-03T02:15:00+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash: 9b04649
category: Changed
depends_on: []
---

# Passphrase prompt must use /dev/tty so piped-stdin commands can prompt

First-user finding (owner, v0.0.14, 2026-07-03). The DOCUMENTED happy path fails at step 2 on a
terminal:

```
$ qfs init a@qmu.jp                                   # prompts, creates the vault — OK
$ cat ~/.config/gmail-ftp/credentials.json | qfs app add google
qfs: error: QFS_PASSPHRASE is not set — export it (or run qfs in a terminal to be prompted) ...
```

The user IS in a terminal; the error's own suggestion is wrong for this invocation.

## Root cause

- `crate::tty::is_interactive()` = `stdin.is_terminal() && stderr.is_terminal()`. Every
  pipe-a-secret command (`app add`, `account add` token import, `account rotate`) has a PIPE on
  stdin **by design** — the credential rides stdin — so the passphrase prompt is refused exactly
  on the flows that need it most.
- Even if allowed, `tty::prompt_secret` reads via `rpassword::read_password()` = **stdin**, which
  would consume the piped credential bytes, not the passphrase.

## Fix

1. Prompt from the **controlling terminal** (`/dev/tty`), not stdin — `rpassword::prompt_password`
   (or `read_password_from_tty`) does this; sudo/ssh behavior. Then the interactivity gate for the
   PASSPHRASE becomes "stderr is a terminal AND /dev/tty opens", independent of stdin.
2. Keep `stdin_is_terminal()` as-is where it decides secret-ENTRY vs secret-PIPE (that gate is
   about the credential, not the passphrase).
3. Reword the `QFS_PASSPHRASE is not set` error for the genuinely non-interactive case only.

## Key files

- `packages/qfs/crates/qfs/src/tty.rs` (`is_interactive`, `prompt_secret`,
  `prompt_secret_confirmed`)
- `packages/qfs/crates/qfs/src/connection.rs` (`resolve_store_passphrase` — the caller gating on
  `is_interactive`)

## Considerations

- The per-one-shot prompt remains once-per-invocation (the in-process cache dies with the
  process); on headless hosts without a secret service the practical path stays
  `read -rs QFS_PASSPHRASE; export`. Documenting that explicitly in getting-started's connect
  section belongs to this ticket too (the smoke always exported the var, which is why the
  docs-are-true check missed this).
- Hermetic test: a child process with stdin=pipe + a pseudo-tty on /dev/tty is hard in CI; at
  minimum unit-test the new gate decision and cover the piped path in the PTY-based e2e test
  (`tty_default_is_table_via_pty` shows the harness pattern).

## Quality Gate

- On a terminal: `cat credentials.json | qfs app add google` prompts for the passphrase on
  /dev/tty, seals the app credentials, and never consumes credential bytes as the passphrase.
- Non-interactive (no /dev/tty): the current clear `QFS_PASSPHRASE` error remains.
- Workspace tests / clippy / fmt green.
