//! `cfs-cmd` — the command layer (RFD-0001 §7).
//!
//! Parses argv with clap-derive and dispatches one of three arms into the shared
//! [`cfs_core`] engine:
//! - **interactive shell** (no subcommand) — the FTP-like prompt (RFD §7);
//! - `cfs run '<stmt>'` / `-e` — one-shot statement execution (RFD §7);
//! - `cfs serve <config.cfs>` — boot the server (RFD §8).
//!
//! Every arm returns a structured [`cfs_core::CfsError::NotImplemented`] at E0 (no
//! panics, no `unwrap`/`expect`). This crate holds **no domain logic** (fidelity
//! guard G5): it depends on `cfs-core` and `cfs-server` only and never reaches past
//! `cfs-core` into `cfs-lang` / `cfs-plan` / `cfs-driver` / `cfs-codec` /
//! `cfs-parser` (acceptance criterion C4, enforced by `tests/dep_direction.rs`).
//!
//! Structured `tracing` is initialised once here, at the command boundary only.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::ffi::OsString;
use std::path::PathBuf;

use cfs_core::{CfsError, Engine, OutputMode, Session};
use clap::{Parser, Subcommand};

/// cfs — one binary that is both a CLI and a server, exposing every external
/// service through one uniform, filesystem-shaped, pipe-SQL DSL (RFD-0001 §1).
#[derive(Parser, Debug)]
#[command(
    name = "cfs",
    version,
    about = "cfs: an AI-driven, DSL-programmable multi-service control plane",
    after_help = "With no subcommand, cfs starts the interactive FTP-like shell (RFD-0001 §7)."
)]
struct Cli {
    /// Emit machine-readable JSON instead of human output (RFD-0001 §5/§7).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Command>,
}

/// The cfs subcommands (RFD-0001 §7/§8).
#[derive(Subcommand, Debug)]
enum Command {
    /// Run one statement and exit (one-shot; absolute paths, no cwd).
    Run {
        /// The statement to execute, e.g. `FROM /mail/inbox |> SELECT subject`.
        #[arg(short = 'e', long = "stmt")]
        stmt: String,
    },
    /// Start the server from a `.cfs` config file (RFD-0001 §8).
    Serve {
        /// Path to the `.cfs` server config.
        config: PathBuf,
    },
    /// Manage stored credentials per driver/account (t27, RFD-0001 §10). The account
    /// *name* is metadata (safe to print); the credential itself is never echoed.
    Account {
        #[command(subcommand)]
        verb: AccountVerb,
    },
    // The absence of a subcommand starts the interactive shell (handled in `run`).
}

/// `cfs account <verb>` — the credential-store management verbs (t27). Each maps onto a
/// [`cfs_core::Secrets`] backend + the resolution model; the credential value is read
/// from a prompt / stdin (never an argv, which would leak into shell history and `ps`).
#[derive(Subcommand, Debug)]
enum AccountVerb {
    /// Add (or replace) the credential for a driver's named account.
    Add {
        /// The driver this account belongs to, e.g. `mail`, `s3`.
        driver: String,
        /// The account name, e.g. `work`, `personal`.
        account: String,
    },
    /// List configured accounts (optionally for one driver). Prints selectors + metadata
    /// only — never a credential.
    List {
        /// Restrict the listing to one driver.
        driver: Option<String>,
    },
    /// Set the persistent active account for a driver (`account use`).
    Use {
        /// The driver to set the active account for.
        driver: String,
        /// The account to make active.
        account: String,
    },
    /// Remove the credential for a driver's named account (idempotent).
    Remove {
        /// The driver.
        driver: String,
        /// The account to remove.
        account: String,
    },
}

/// The library entrypoint the thin `cfs` binary calls. Parses `args`, dispatches,
/// and maps the outcome to a process exit code (`0` on success, `1` on a structured
/// error, `2` on argv/usage errors from clap). Never panics.
///
/// Returns the intended process exit code; the binary forwards it to
/// `std::process::exit`.
#[must_use]
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    init_tracing();

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            // clap renders help/version/usage. Print to the correct stream and use
            // clap's own exit-code convention (0 for --help/--version, 2 for usage).
            let _ = err.print();
            return err.exit_code();
        }
    };

    let output = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };

    // The Engine and Session are constructed here and threaded into dispatch; at E0
    // they carry only empty registries (the seam the later epics fill).
    let engine = Engine::new();
    let mut session = Session::new();
    session.output = output;

    let outcome = match cli.cmd {
        Some(Command::Run { stmt }) => dispatch_run(&engine, &session, &stmt),
        Some(Command::Serve { config }) => cfs_server::serve(&config),
        Some(Command::Account { verb }) => dispatch_account(&engine, &session, &verb),
        None => dispatch_shell(&engine, &session),
    };

    match outcome {
        Ok(()) => 0,
        Err(err) => {
            report_error(&err, output);
            1
        }
    }
}

/// Dispatch `cfs run '<stmt>'`. E0: structured not-implemented (no domain logic).
fn dispatch_run(_engine: &Engine, _session: &Session, stmt: &str) -> Result<(), CfsError> {
    tracing::debug!(target: "cfs::cmd", %stmt, "dispatch run (stub)");
    Err(CfsError::NotImplemented { feature: "run" })
}

/// Dispatch the interactive shell. E0: structured not-implemented (no domain logic).
fn dispatch_shell(_engine: &Engine, _session: &Session) -> Result<(), CfsError> {
    tracing::debug!(target: "cfs::cmd", "dispatch interactive shell (stub)");
    Err(CfsError::NotImplemented { feature: "shell" })
}

/// Dispatch `cfs account <verb>` (t27). The credential store + resolver substrate this
/// drives lives in `cfs-secrets` (consumed via [`cfs_core::Secrets`]); the verb surface
/// is declared here and the credential-bearing I/O (prompt → keyring/passphrase →
/// encrypted backend) is the parked seam the keyring-plumbing follow-up fills. Matches the
/// E0 stub pattern of the other arms: a structured, secret-free `NotImplemented` per verb.
/// No credential is ever read from argv (it would leak into shell history / `ps`).
fn dispatch_account(
    _engine: &Engine,
    _session: &Session,
    verb: &AccountVerb,
) -> Result<(), CfsError> {
    let feature = match verb {
        AccountVerb::Add { .. } => "account add",
        AccountVerb::List { .. } => "account list",
        AccountVerb::Use { .. } => "account use",
        AccountVerb::Remove { .. } => "account remove",
    };
    tracing::debug!(target: "cfs::cmd", feature, "dispatch account (stub)");
    Err(CfsError::NotImplemented { feature })
}

/// Render a [`CfsError`] to stderr: a human line, or a `{"error": {...}}` JSON
/// envelope (AI-facing, RFD §5). This is the only place output mode is rendered.
fn report_error(err: &CfsError, output: OutputMode) {
    match output {
        OutputMode::Human => {
            eprintln!("error[{}]: {err}", err.code());
        }
        OutputMode::Json => {
            // Hand-built envelope: no serde dependency needed for two string fields,
            // and the strings here are controlled (codes are stable identifiers,
            // the message escapes quotes/backslashes).
            let message = escape_json(&err.to_string());
            eprintln!(
                "{{\"error\":{{\"code\":\"{}\",\"message\":\"{}\"}}}}",
                err.code(),
                message
            );
        }
    }
}

/// Minimal JSON string escaping for the error envelope.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Initialise structured logging at the command boundary only. Idempotent: a second
/// call is a no-op (the global subscriber is already set). Reads `RUST_LOG`.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // `try_init` returns Err if a global subscriber is already set; ignore it so
    // repeated calls (e.g. in tests) do not panic.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_subcommand_dispatch_is_not_implemented() {
        let engine = Engine::new();
        let session = Session::new();
        let err = dispatch_run(&engine, &session, "FROM /mail").unwrap_err();
        assert!(matches!(err, CfsError::NotImplemented { feature: "run" }));
    }

    #[test]
    fn shell_dispatch_is_not_implemented() {
        let engine = Engine::new();
        let session = Session::new();
        let err = dispatch_shell(&engine, &session).unwrap_err();
        assert!(matches!(err, CfsError::NotImplemented { feature: "shell" }));
    }

    #[test]
    fn run_returns_exit_code_one_on_not_implemented() {
        // `cfs run -e 'anything'` reaches a structured error, not a panic; exit 1.
        let code = run(["cfs", "run", "-e", "anything"]);
        assert_eq!(code, 1);
    }

    #[test]
    fn serve_returns_exit_code_one_on_not_implemented() {
        let code = run(["cfs", "serve", "x.cfs"]);
        assert_eq!(code, 1);
    }

    #[test]
    fn account_verbs_dispatch_to_structured_not_implemented() {
        let engine = Engine::new();
        let session = Session::new();
        let cases = [
            (
                AccountVerb::Add {
                    driver: "mail".into(),
                    account: "work".into(),
                },
                "account add",
            ),
            (AccountVerb::List { driver: None }, "account list"),
            (
                AccountVerb::Use {
                    driver: "mail".into(),
                    account: "work".into(),
                },
                "account use",
            ),
            (
                AccountVerb::Remove {
                    driver: "mail".into(),
                    account: "work".into(),
                },
                "account remove",
            ),
        ];
        for (verb, feature) in cases {
            let err = dispatch_account(&engine, &session, &verb).unwrap_err();
            match err {
                CfsError::NotImplemented { feature: f } => assert_eq!(f, feature),
                other => panic!("expected NotImplemented({feature}), got {other:?}"),
            }
        }
    }

    #[test]
    fn account_subcommand_parses_and_exits_one() {
        // `cfs account list` parses cleanly and reaches the structured stub (exit 1).
        assert_eq!(run(["cfs", "account", "list"]), 1);
        assert_eq!(run(["cfs", "account", "add", "mail", "work"]), 1);
    }

    #[test]
    fn no_subcommand_runs_shell_stub_exit_one() {
        let code = run(["cfs"]);
        assert_eq!(code, 1);
    }

    #[test]
    fn help_exits_zero_without_panic() {
        let code = run(["cfs", "--help"]);
        assert_eq!(code, 0);
    }

    #[test]
    fn json_error_envelope_is_valid_json() {
        // The JSON envelope must be parseable (AI-facing contract, RFD §5).
        let err = CfsError::NotImplemented { feature: "run" };
        // Re-derive the envelope the way report_error builds it.
        let envelope = format!(
            "{{\"error\":{{\"code\":\"{}\",\"message\":\"{}\"}}}}",
            err.code(),
            escape_json(&err.to_string())
        );
        let parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert_eq!(parsed["error"]["code"], "not_implemented");
        assert!(parsed["error"]["message"].is_string());
    }
}
