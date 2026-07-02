//! Interactive-terminal helpers — the human-facing side of credential entry.
//!
//! qfs is **stdin-pipe-first**: agents and scripts pipe secrets (never argv), and the credential
//! path reads them from stdin. That is perfect for automation but makes a *human's* first run a
//! copy-paste chore (`read -rs QFS_PASSPHRASE; export …` before every fresh pane).
//! These helpers add the missing INTERACTIVE path: when qfs is attached to a real terminal it
//! *prompts* — with echo disabled — instead of requiring the env var or a piped password.
//!
//! Two invariants keep this safe and automation-transparent:
//! - Every prompt is gated: values read from **stdin** ([`prompt_line`], the piped-credential
//!   entry) gate on [`is_interactive`] / [`stdin_is_terminal`]; the PASSPHRASE prompt reads the
//!   **controlling terminal** (`/dev/tty` — rpassword's default input), so it gates on
//!   [`can_prompt_secret`] instead and works even while stdin carries a piped secret
//!   (`cat credentials.json | qfs app add google`). With no controlling terminal (cron, CI) no
//!   prompt is ever reached and callers keep their non-interactive behavior.
//! - The prompt text is written to **stderr**, never stdout, so it never pollutes captured output;
//!   the secret is read with terminal echo OFF and lands in a zeroizing [`Secret`], never on argv,
//!   stdout, or the environment.

use qfs_secrets::Secret;
use std::io::{IsTerminal, Write};

/// True only when BOTH stdin and stderr are real terminals — the guard every *prompt-and-continue*
/// flow that reads its answer from STDIN checks (e.g. the init email prompt), so redirected output
/// or piped input never blocks on an invisible prompt.
#[must_use]
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Whether the PASSPHRASE prompt can run: stderr is a terminal (to carry the prompt text) and a
/// controlling terminal exists for the answer. **Deliberately independent of stdin**: the
/// pipe-a-secret commands (`app add`, `account add` token import, `account rotate`) carry the
/// credential on stdin BY DESIGN, and [`prompt_secret`] reads the passphrase from `/dev/tty`,
/// never stdin — so a piped stdin must not disable the prompt (the v0.0.14 first-run regression).
#[must_use]
pub fn can_prompt_secret() -> bool {
    std::io::stderr().is_terminal() && controlling_tty_available()
}

/// Whether a controlling terminal is reachable for the echo-off secret read. On unix this is
/// exactly "does `/dev/tty` open" (the path rpassword reads); elsewhere fall back to stdin being
/// a terminal (the pre-/dev/tty behavior — release targets are unix-only today).
#[cfg(unix)]
fn controlling_tty_available() -> bool {
    std::fs::File::open("/dev/tty").is_ok()
}

#[cfg(not(unix))]
fn controlling_tty_available() -> bool {
    std::io::stdin().is_terminal()
}

/// Whether stdin alone is a terminal — the gate for the password-entry fallback, where stdin is the
/// very thing we'd otherwise read a piped secret from.
#[must_use]
pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

/// Prompt on stderr and read one secret line from the **controlling terminal** (`/dev/tty`,
/// rpassword's default input) with terminal echo OFF — never from stdin, which may be carrying a
/// piped credential. The plaintext never touches stdout, argv, or the environment; it is returned
/// as a zeroizing [`Secret`].
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

// ---- The paste-back Google consent interaction (mirrors gmail-ftp) ------------------------

/// Run the terminal side of the paste-back browser consent: print the consent URL, offer
/// `c` = copy it to the user's LOCAL clipboard via the OSC 52 escape (the one channel that
/// reaches a laptop across SSH + tmux) and `o` = best-effort open a browser on THIS host, then
/// read back the `http://localhost/?state=…&code=…` redirect URL (or the bare `code=` value)
/// the user pastes. All prompt text rides stderr; the answers are read from the controlling
/// terminal (`/dev/tty`) so a piped stdin never swallows them. The pasted line goes back to
/// `qfs_google_auth::authorize`, which validates `state` before any exchange.
///
/// # Errors
/// A string message if the terminal cannot be read.
pub fn consent_paste_prompt(auth_url: &str) -> Result<String, String> {
    eprintln!("\nTo authorize qfs, open this URL in your LOCAL browser:\n\n{auth_url}\n");
    let choice = prompt_tty_line(
        "Press 'c' + Enter to copy the URL to your local clipboard, 'o' + Enter to open a \
         browser on this host, or just Enter to copy it yourself: ",
    )?;
    match choice.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('c') => {
            copy_to_clipboard(auth_url);
            eprintln!("Copied to your local clipboard.");
        }
        Some('o') => open_browser(auth_url),
        _ => {}
    }
    eprint!(
        "\nAfter you authorize, the browser redirects to a http://localhost/... URL that fails \
         to load.\nPaste that entire URL here (or just the code= value): "
    );
    let _ = std::io::stderr().flush();
    // Skip stray blank lines (a leftover newline from the choice prompt) like gmail-ftp does.
    loop {
        let line = read_tty_line()?;
        if !line.trim().is_empty() {
            return Ok(line);
        }
    }
}

/// Prompt on stderr and read one echoed line from the controlling terminal. Unlike the
/// paste read, an empty line is a valid answer (the "copy it yourself" fall-through).
fn prompt_tty_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    read_tty_line()
}

/// Read one echoed line from the controlling terminal (`/dev/tty`), so a piped stdin never
/// swallows the answer; with no controlling terminal, fall back to stdin. Reads byte-by-byte
/// (a canonical-mode tty delivers at most one line per read; no read-ahead is buffered away).
fn read_tty_line() -> Result<String, String> {
    use std::io::Read;
    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::File::open("/dev/tty") {
        let mut line = Vec::new();
        loop {
            let mut b = [0_u8; 1];
            let n = tty
                .read(&mut b)
                .map_err(|e| format!("reading from the terminal: {e}"))?;
            if n == 0 || b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
        }
        let text = String::from_utf8_lossy(&line);
        return Ok(text.trim_end_matches('\r').to_string());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|e| format!("reading from the terminal: {e}"))?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

/// Put `text` on the user's LOCAL clipboard via the OSC 52 terminal escape, which the terminal
/// emulator honors even across SSH — deliberately NOT xclip/pbcopy (those target this host, not
/// the user's machine). Written to the controlling terminal so it works even when stderr is
/// redirected; best-effort (the URL is also printed for manual copy).
fn copy_to_clipboard(text: &str) {
    let seq = clipboard_seq(text, std::env::var_os("TMUX").is_some());
    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(seq.as_bytes());
        return;
    }
    let _ = std::io::stderr().write_all(seq.as_bytes());
}

/// Build the OSC 52 escape that sets the clipboard to `text` (base64-encoded per the spec).
/// Inside tmux the sequence is wrapped in the DCS passthrough (DCS prefix, every ESC doubled,
/// ST terminator) so it reaches the outer terminal instead of being swallowed. Pure, so tests
/// cover both shapes directly.
fn clipboard_seq(text: &str, in_tmux: bool) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    }
}

/// Best-effort open `url` in a browser on THIS host (`o`). Failure is silently ignored — the
/// URL is printed for manual use, and on an SSH host there is usually no browser to open.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    const OPENER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const OPENER: &str = "xdg-open";
    let _ = std::process::Command::new(OPENER)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::clipboard_seq;
    use base64::Engine as _;

    /// The OSC 52 escape carries the base64 of the text between the `]52;c;` selector and the
    /// BEL terminator.
    #[test]
    fn clipboard_seq_is_osc52_with_base64_payload() {
        let url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=x";
        let seq = clipboard_seq(url, false);
        let b64 = base64::engine::general_purpose::STANDARD.encode(url.as_bytes());
        assert_eq!(seq, format!("\x1b]52;c;{b64}\x07"));
    }

    /// Inside tmux the OSC 52 escape is wrapped in the DCS passthrough: DCS `tmux;` prefix,
    /// every ESC doubled, ST terminator — so the escape reaches the OUTER terminal.
    #[test]
    fn clipboard_seq_wraps_in_tmux_dcs_passthrough() {
        let seq = clipboard_seq("x", true);
        assert!(seq.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(seq.ends_with("\x07\x1b\\"));
        // The inner escape's ESC is doubled; only the DCS frame's own ESCs are single.
        let inner = &seq["\x1bPtmux;".len()..seq.len() - "\x1b\\".len()];
        assert!(!inner.replace("\x1b\x1b", "").contains('\x1b'));
    }
}
