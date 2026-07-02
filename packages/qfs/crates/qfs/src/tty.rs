//! Interactive-terminal helpers — the human-facing side of credential entry.
//!
//! qfs is **stdin-pipe-first**: agents and scripts pipe secrets (never argv), and the credential
//! path reads them from stdin. That is perfect for automation but makes a *human's* first run a
//! copy-paste chore (`read -rs QFS_PASSPHRASE; export …`, `printf %s "$PW" | qfs identity signup …`).
//! These helpers add the missing INTERACTIVE path: when qfs is attached to a real terminal it
//! *prompts* — with echo disabled — instead of requiring the env var or a piped password.
//!
//! Two invariants keep this safe and automation-transparent:
//! - Every prompt is gated by [`is_interactive`] / [`stdin_is_terminal`]. When stdin isn't a TTY
//!   (a pipe, cron, CI) these are never reached and the caller keeps its non-interactive behavior.
//! - The prompt text is written to **stderr**, never stdout, so it never pollutes captured output;
//!   the secret is read with terminal echo OFF and lands in a zeroizing [`Secret`], never on argv,
//!   stdout, or the environment.

use qfs_secrets::Secret;
use std::io::{IsTerminal, Write};

/// True only when BOTH stdin and stderr are real terminals — the guard every *prompt-and-continue*
/// flow checks so redirected output or piped input never blocks on an invisible prompt.
#[must_use]
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Whether stdin alone is a terminal — the gate for the password-entry fallback, where stdin is the
/// very thing we'd otherwise read a piped secret from.
#[must_use]
pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

/// Prompt on stderr and read one secret line with terminal echo OFF. The plaintext never touches
/// stdout, argv, or the environment; it is returned as a zeroizing [`Secret`].
///
/// # Errors
/// A string message if the terminal cannot be read.
pub fn prompt_secret(prompt: &str) -> Result<Secret, String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let entered =
        rpassword::read_password().map_err(|e| format!("reading from the terminal: {e}"))?;
    Ok(Secret::from(entered.as_str()))
}

/// Prompt for a secret twice and require the two entries to match — used when a NEW secret is being
/// SET (a fresh store passphrase, a sign-up password) so a typo can't silently lock the user out of
/// their own store. Rejects an empty entry.
///
/// # Errors
/// A string message if the terminal cannot be read, the entry is empty, or the two entries differ.
pub fn prompt_secret_confirmed(prompt: &str, confirm: &str) -> Result<Secret, String> {
    let first = prompt_secret(prompt)?;
    if first.expose().is_empty() {
        return Err("an empty passphrase is not allowed — nothing was saved".into());
    }
    let second = prompt_secret(confirm)?;
    if first.expose() != second.expose() {
        return Err("the two entries did not match — nothing was saved, try again".into());
    }
    Ok(first)
}

/// Prompt on stderr and read one ECHOED line — for a NON-secret value such as an email address.
/// The trailing newline is trimmed.
///
/// # Errors
/// A string message if stdin cannot be read.
pub fn prompt_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|e| format!("reading from the terminal: {e}"))?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}
