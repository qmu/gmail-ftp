//! A process-global "colorize human output" flag plus the few ANSI codes the renderers use.
//!
//! The CLI decides ONCE at startup whether to colorize — `stdout.is_terminal()` AND the `NO_COLOR`
//! env var is unset AND `--no-color` was not passed — and calls [`set_enabled`]. The human
//! renderers (the table formatter, the preview's irreversible marker, the error line) consult
//! [`enabled`] and wrap text via [`paint`]. JSON output never colorizes (it never calls `paint`).
//!
//! Hand-rolled ANSI (no `owo-colors`/`termcolor` crate) on the same anti-heavy-dep precedent as the
//! in-house table formatter (ADR-0002/0003): a handful of SGR escapes is all the CLI needs.

use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide flag: `true` ⇒ human renderers emit ANSI color. Defaults to `false` (no color)
/// so any non-CLI consumer (tests, the server, embedded use) is colorless unless it opts in.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Set whether human output is colorized, process-wide. The CLI calls this once at startup with
/// the resolved `tty && !NO_COLOR && !--no-color` decision.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether colorized human output is currently enabled.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Reset to a header/label highlight (bold cyan).
pub const HEADER: &str = "\x1b[1;36m";
/// An error highlight (bold red).
pub const ERROR: &str = "\x1b[1;31m";
/// An irreversible / warning highlight (bold yellow).
pub const WARN: &str = "\x1b[1;33m";
/// The SGR reset sequence.
pub const RESET: &str = "\x1b[0m";

/// Wrap `text` in the SGR `code` … reset when color is [`enabled`]; otherwise return `text`
/// unchanged. The one place an ANSI escape is emitted, so disabling color is total.
#[must_use]
pub fn paint(code: &str, text: &str) -> String {
    if enabled() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_is_a_noop_when_disabled_and_wraps_when_enabled() {
        // Serialized within this test; the flag is process-global so we set both states here.
        set_enabled(false);
        assert_eq!(paint(HEADER, "name"), "name");
        set_enabled(true);
        assert_eq!(paint(HEADER, "name"), format!("{HEADER}name{RESET}"));
        // Restore the default so other tests in this binary see no color.
        set_enabled(false);
    }
}
