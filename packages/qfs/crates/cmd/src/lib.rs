//! `qfs-cmd` — the command layer (RFD-0001 §7).
//!
//! Parses argv with clap-derive and dispatches one of three arms into the shared
//! [`qfs_core`] engine:
//! - **interactive shell** (no subcommand) — the FTP-like prompt (RFD §7);
//! - `qfs run '<stmt>'` / `-e` — one-shot statement execution (RFD §7);
//! - `qfs serve <config.qfs>` — boot the server (RFD §8).
//!
//! Every arm returns a structured [`qfs_core::CfsError::NotImplemented`] at E0 (no
//! panics, no `unwrap`/`expect`). This crate holds **no domain logic** (fidelity
//! guard G5): it depends on `qfs-core` and `qfs-server` only and never reaches past
//! `qfs-core` into `qfs-lang` / `qfs-plan` / `qfs-driver` / `qfs-codec` /
//! `qfs-parser` (acceptance criterion C4, enforced by `tests/dep_direction.rs`).
//!
//! Structured `tracing` is initialised once here, at the command boundary only.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use qfs_core::{CfsError, Engine, OutputMode, Session};

mod redact;

/// The interactive-shell launcher the binary injects (t28). The shell's REAL local-FS read
/// facet lives in the **binary** crate, not here: that adapter depends on `qfs-driver-local`,
/// which is a `qfs-runtime` consumer, so a `qfs-cmd → qfs-driver-local` edge would make qfs-cmd
/// a non-leaf runtime consumer and (correctly) fail the runtime-confinement guard. Injecting the
/// launcher keeps qfs-cmd off both the runtime and the driver crates: qfs-cmd only knows "no
/// subcommand → call the launcher", and the leaf binary (an allowlisted runtime consumer)
/// supplies the registry wiring + REPL driver. Returns the process exit code.
pub type ShellLauncher<'a> = dyn Fn() -> i32 + 'a;

/// The injected **serve launcher** (t32): the binary supplies `qfs serve <config>` so the
/// HTTP serving binding (`qfs-http`, a leaf that consumes both `qfs-server` and the `qfs-exec`
/// read executor) lives in the binary composition root — NOT in qfs-cmd, which must stay off
/// qfs-exec/qfs-http (the dep_direction guards). qfs-cmd only knows "the `serve` subcommand →
/// call the launcher with the config path"; the leaf binary wires the `Runtime` + `HttpBinding`
/// + listener and returns the process exit code.
pub type ServeLauncher<'a> = dyn Fn(&std::path::Path) -> i32 + 'a;

/// The injected **describe-registry provider** (t39): the binary supplies the
/// [`qfs_core::MountRegistry`] of **describe-only drivers** (each driver's pure introspective
/// facet, constructed cred-free) that `qfs describe <path>` consults. It lives in the binary
/// composition root — NOT in qfs-cmd, which must stay off the concrete driver crates (the
/// dep_direction guard forbids qfs-cmd a `qfs-driver-*` edge; the binary is the allowlisted leaf
/// that may carry them). qfs-cmd only knows "the `describe` subcommand → build the registry via
/// this provider, then hand it + the path to `qfs_exec::run_describe`". DESCRIBE is PURE (no
/// creds, no I/O, no network), so the registry holds describe-only drivers and the applier seam
/// is never reached.
pub type DescribeProvider<'a> = dyn Fn() -> qfs_core::MountRegistry + 'a;

/// The injected **skill provider** (t39 CO-t39-1): the binary supplies the embedded agent skill
/// text (`qfs_skill::render`) that `qfs skill` prints. It lives in the binary composition root —
/// NOT in qfs-cmd, which stays logic-free — so the `qfs → qfs-skill` NORMAL dep edge (the edge that
/// makes `SKILL.md` genuinely SHIP in the binary artifact rather than get dead-stripped) lands on
/// the terminal binary, and qfs-cmd only knows "the `skill` subcommand → call this with the
/// `--examples` flag → print the returned text". `qfs-skill` has an empty `[dependencies]`, so the
/// edge adds zero transitive runtime weight. The argument is `include_examples`.
pub type SkillProvider<'a> = dyn Fn(bool) -> String + 'a;

/// qfs — one binary that is both a CLI and a server, exposing every external
/// service through one uniform, filesystem-shaped, pipe-SQL DSL (RFD-0001 §1).
#[derive(Parser, Debug)]
#[command(
    name = "qfs",
    version,
    about = "qfs: an AI-driven, DSL-programmable multi-service control plane",
    after_help = "With no subcommand, qfs starts the interactive FTP-like shell (RFD-0001 §7)."
)]
struct Cli {
    /// Emit machine-readable JSON instead of human output (RFD-0001 §5/§7).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Command>,
}

/// The qfs subcommands (RFD-0001 §7/§8).
#[derive(Subcommand, Debug)]
enum Command {
    /// Run one statement and exit (one-shot; absolute paths, no cwd).
    ///
    /// Exactly one statement source: a positional `qfs run '<stmt>'`, `-e <stmt>`, or `-`
    /// (read the statement from stdin). PREVIEW by default; `--commit` (or a trailing
    /// `COMMIT`) applies an effect plan.
    Run {
        /// The statement to execute positionally, e.g. `FROM /mail/inbox |> SELECT subject`.
        /// Use `-` to read the statement from stdin. Mutually exclusive with `-e`.
        stmt: Option<String>,
        /// The statement to execute (the `-e <stmt>` form). Mutually exclusive with the
        /// positional form and stdin.
        #[arg(short = 'e', long = "expr")]
        expr: Option<String>,
        /// Output format: `json` or `table`. Default: `table` on a TTY, `json` when piped.
        #[arg(long = "format", value_name = "FORMAT")]
        format: Option<String>,
        /// Apply an effect plan (default is PREVIEW). A trailing `COMMIT` keyword has the
        /// same effect; this is only the apply switch (the CLI adds zero keywords).
        #[arg(long = "commit")]
        commit: bool,
        /// Acknowledge applying an irreversible effect (a `REMOVE` / `CALL mail.send`) in this
        /// non-interactive one-shot. Without it, a `--commit` of an irreversible plan fails
        /// closed (t37, RFD §6/§10): a one-shot has no TTY to confirm on, so the ack must be
        /// explicit. No effect on a reversible plan.
        #[arg(long = "commit-irreversible")]
        commit_irreversible: bool,
        /// Suppress progress output; never suppresses the error body.
        #[arg(long = "quiet", short = 'q')]
        quiet: bool,
    },
    /// Describe a node: its archetype, columns, supported verbs, `CALL` procedures, prelude
    /// aliases, and pushdown — the agent's first loop step (t39, RFD §5).
    ///
    /// `DESCRIBE` is PURE: no credentials, no I/O, no network. It reads only the driver's
    /// introspective contract, so `qfs describe /mail/drafts -json` resolves offline. The agent
    /// reads this report, writes a qfs statement, PREVIEWs it, then COMMITs.
    Describe {
        /// The node to describe, e.g. `/mail/drafts`. Absolute path or `id:` form (no cwd).
        path: String,
        /// Output format: `json` or `table`. Default: `table` on a TTY, `json` when piped.
        #[arg(long = "format", value_name = "FORMAT")]
        format: Option<String>,
    },
    /// Print the embedded AI operating-procedure skill (`SKILL.md`) and exit (t39).
    ///
    /// This is how an AI agent discovers the uniform loop — DESCRIBE → write a qfs statement →
    /// PREVIEW → COMMIT — directly from the running binary (the skill ships embedded via
    /// `include_str!`). `--examples` also dumps the worked-example corpus (one per driver).
    Skill {
        /// Also print the embedded worked-example corpus (one canonical example per driver).
        #[arg(long = "examples")]
        examples: bool,
    },
    /// Start the server from a `.qfs` config file (RFD-0001 §8).
    Serve {
        /// Path to the `.qfs` server config.
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

/// `qfs account <verb>` — the credential-store management verbs (t27). Each maps onto a
/// [`qfs_core::Secrets`] backend + the resolution model; the credential value is read
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

/// The library entrypoint the thin `qfs` binary calls. Parses `args`, dispatches,
/// and maps the outcome to a process exit code (`0` on success, `1` on a structured
/// error, `2` on argv/usage errors from clap). Never panics.
///
/// The no-subcommand interactive shell is launched via the injected [`ShellLauncher`] (the
/// binary supplies the runtime-coupled local read facet + REPL driver). Returns the intended
/// process exit code; the binary forwards it to `std::process::exit`.
#[must_use]
pub fn run<I, T>(
    args: I,
    shell: &ShellLauncher,
    serve: &ServeLauncher,
    describe: &DescribeProvider,
    skill: &SkillProvider,
) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    init_tracing();

    // Accept the RFD/ticket shorthand `-json` (single dash) as an alias for the canonical global
    // `--json` flag. Clap would otherwise lex `-json` as the bundled short flags `-j -s -o -n`;
    // rewriting the single, exact token `-json` → `--json` keeps the documented surface
    // (`qfs describe /mail/drafts -json`) working without inventing single-char flags.
    let args = normalize_json_alias(args);

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

    // `qfs run` owns its own exit-code contract (t29), so it is dispatched separately: the
    // execution layer (qfs-exec) renders rows/plan to stdout and the structured error to
    // stderr, returning the stable exit code directly.
    if let Some(Command::Run {
        stmt,
        expr,
        format,
        commit,
        commit_irreversible,
        quiet,
    }) = &cli.cmd
    {
        return dispatch_run(
            &engine,
            RunOpts {
                stmt: stmt.clone(),
                expr: expr.clone(),
                format: format.clone(),
                json: cli.json,
                commit: *commit,
                commit_irreversible: *commit_irreversible,
                quiet: *quiet,
            },
        );
    }

    // `qfs describe` owns its own exit-code contract (t39, same as `qfs run`): it renders the
    // DescribeReport / structured error directly through the t29 output layer and returns the
    // stable exit code. The describe-only driver registry is built by the injected provider (the
    // binary composition root that owns the concrete driver crates); qfs-cmd stays off them.
    if let Some(Command::Describe { path, format }) = &cli.cmd {
        return dispatch_describe(path, format.as_deref(), cli.json, describe);
    }

    // `qfs skill` prints the embedded operating procedure (and optionally the example corpus) and
    // exits 0. Logic-free: the binary owns the `qfs-skill` const (the NORMAL dep edge that makes the
    // skill genuinely ship in the artifact); qfs-cmd only routes to the injected provider.
    if let Some(Command::Skill { examples }) = &cli.cmd {
        print!("{}", skill(*examples));
        return 0;
    }

    // No subcommand → the interactive shell, run by the injected launcher (which owns the
    // runtime-coupled local read facet + REPL driver; see [`ShellLauncher`]). It returns the
    // process exit code directly.
    if cli.cmd.is_none() {
        tracing::debug!(target: "qfs::cmd", "dispatch interactive shell via launcher");
        return shell();
    }

    let outcome = match cli.cmd {
        // Handled above; unreachable here but kept total.
        Some(Command::Run { .. })
        | Some(Command::Describe { .. })
        | Some(Command::Skill { .. })
        | None => Ok(()),
        // `serve` is dispatched through the injected launcher (the binary composition root that
        // wires the HTTP binding); it returns the process exit code directly.
        Some(Command::Serve { config }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch serve via launcher");
            return serve(&config);
        }
        Some(Command::Account { verb }) => dispatch_account(&engine, &session, &verb),
    };

    match outcome {
        Ok(()) => 0,
        Err(err) => {
            report_error(&err, output);
            1
        }
    }
}

/// Rewrite the exact argv token `-json` (single dash) to the canonical `--json` flag, leaving
/// every other argument untouched. The RFD and the t39 ticket write `qfs … -json`; clap's lexer
/// treats `-json` as bundled single-char flags (`-j -s -o -n`), so this one-token normalization
/// preserves the documented surface without adding spurious short flags. Only the standalone,
/// exact `-json` token is rewritten — `--json`, `-j`-style bundles a user actually typed, and any
/// value equal to `-json` after a `--` separator are left as-is (we stop at the first `--`).
fn normalize_json_alias<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut out = Vec::new();
    let mut passthrough = false;
    for arg in args {
        let os: OsString = arg.into();
        if passthrough {
            out.push(os);
            continue;
        }
        if os == *"--" {
            passthrough = true;
            out.push(os);
        } else if os == *"-json" {
            out.push(OsString::from("--json"));
        } else {
            out.push(os);
        }
    }
    out
}

/// The resolved options for one `qfs run` invocation.
struct RunOpts {
    stmt: Option<String>,
    expr: Option<String>,
    format: Option<String>,
    json: bool,
    commit: bool,
    commit_irreversible: bool,
    quiet: bool,
}

/// Dispatch `qfs run` (t29): resolve the single statement source (positional / `-e` / `-`
/// stdin), choose the output format (explicit flag wins; else `table` on a TTY, `json` when
/// piped), and hand off to the execution layer, which renders the result and returns the
/// stable exit code. Logic-free: all execution lives in `qfs-exec`.
fn dispatch_run(engine: &Engine, opts: RunOpts) -> i32 {
    use std::io::IsTerminal;

    // Resolve the statement source. A positional `-` means "read from stdin".
    let (positional, stdin_src) = match opts.stmt.as_deref() {
        Some("-") => (None, Some(read_stdin())),
        Some(s) => (Some(s.to_string()), None),
        None => (None, None),
    };
    let source = match qfs_exec::resolve_source(positional, opts.expr.clone(), stdin_src) {
        Ok(s) => s,
        Err(err) => return render_run_error(&err, &resolve_format(&opts, false)),
    };

    // Format: explicit `--format`/`--json` always wins; else default by TTY of stdout.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let fmt = resolve_format(&opts, stdout_is_tty);

    // The read-driver registry. At the cmd boundary it is empty: real driver registration
    // (read facet) is the E4/bootstrap seam this consumes read-only. With no read driver a
    // `FROM /x` resolves to a structured capability error (exit 3) rather than a panic.
    let reads = qfs_exec::ReadRegistry::new();
    let ctx = qfs_exec::ExecCtx {
        engine,
        reads: &reads,
    };

    let _ = opts.quiet; // `--quiet` suppresses progress; the renderers emit no progress yet.

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut streams = qfs_exec::Streams {
        out: &mut out,
        err: &mut err,
    };
    qfs_exec::run_oneshot(
        &source,
        &ctx,
        fmt,
        opts.commit,
        opts.commit_irreversible,
        &mut streams,
    )
    .code()
}

/// Dispatch `qfs describe <path>` (t39): build the describe-only driver registry via the injected
/// provider, resolve the output format (explicit flag wins; else table on a TTY, json when
/// piped), and hand off to `qfs_exec::run_describe`, which folds the driver's introspective half
/// into a [`qfs_core::DescribeReport`] and renders it. Logic-free: all execution lives in
/// `qfs-exec`; the driver wiring lives in the binary (via `describe`).
fn dispatch_describe(
    path: &str,
    format: Option<&str>,
    json: bool,
    describe: &DescribeProvider,
) -> i32 {
    use std::io::IsTerminal;

    let stdout_is_tty = std::io::stdout().is_terminal();
    let fmt = resolve_describe_format(json, format, stdout_is_tty);

    // Build the describe-only registry from the injected provider (the binary composition root).
    let registry = describe();

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut streams = qfs_exec::Streams {
        out: &mut out,
        err: &mut err,
    };
    qfs_exec::run_describe(path, &registry, fmt, &mut streams).code()
}

/// Resolve the describe output format (mirrors `qfs run`): `--json` / `--format json|table` wins;
/// else `table` on a TTY, `json` when piped (deterministic for an agent's scripted pipe).
fn resolve_describe_format(
    json: bool,
    format: Option<&str>,
    stdout_is_tty: bool,
) -> qfs_exec::OutputFormat {
    if json {
        return qfs_exec::OutputFormat::Json;
    }
    match format {
        Some("json") => qfs_exec::OutputFormat::Json,
        Some("table") => qfs_exec::OutputFormat::Table,
        _ if stdout_is_tty => qfs_exec::OutputFormat::Table,
        _ => qfs_exec::OutputFormat::Json,
    }
}

/// Resolve the output format: an explicit `--format json|table` (or the `--json` alias) always
/// wins; otherwise `table` on a TTY, `json` when piped (deterministic for scripts).
fn resolve_format(opts: &RunOpts, stdout_is_tty: bool) -> qfs_exec::OutputFormat {
    if opts.json {
        return qfs_exec::OutputFormat::Json;
    }
    match opts.format.as_deref() {
        Some("json") => qfs_exec::OutputFormat::Json,
        Some("table") => qfs_exec::OutputFormat::Table,
        // Unknown/absent: fall back to the TTY default (an unknown value is treated as the
        // default rather than erroring; clap could restrict this with a value_parser later).
        _ if stdout_is_tty => qfs_exec::OutputFormat::Table,
        _ => qfs_exec::OutputFormat::Json,
    }
}

/// Render a `qfs run` error that occurred before the executor (e.g. bad statement source) and
/// return its exit code.
fn render_run_error(err: &qfs_exec::ExecError, fmt: &qfs_exec::OutputFormat) -> i32 {
    let renderer = fmt.renderer();
    let mut stderr = std::io::stderr();
    let _ = renderer.error(err, &mut stderr);
    err.exit_code().code()
}

/// Read the whole statement from stdin (`qfs run -`). On a read error, returns an empty
/// string, which the parser rejects with a structured parse error (no panic).
fn read_stdin() -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

/// Dispatch `qfs account <verb>` (t27). The credential store + resolver substrate this
/// drives lives in `qfs-secrets` (consumed via [`qfs_core::Secrets`]); the verb surface
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
    tracing::debug!(target: "qfs::cmd", feature, "dispatch account (stub)");
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

/// A `MakeWriter` that wraps stderr and runs every emitted log line through the t37
/// [`redact::scrub`] defense-in-depth pass before it reaches the byte sink — so a secret SHAPE
/// that slipped past the `Secret` type (the primary control) into ANY span/event, from ANY crate,
/// is scrubbed at the one logging seam. See `redact.rs` for what it scans and why it is a backup.
#[derive(Clone, Default)]
struct ScrubMakeWriter;

/// The per-write scrubbing sink. The fmt subscriber writes one fully-rendered event per `write`,
/// so scrubbing each write buffer covers the whole line; partial writes are still individually
/// scrubbed (conservative — it never corrupts a benign line).
struct ScrubWriter;

impl std::io::Write for ScrubWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Render the buffer as text, scrub known secret shapes, and forward to stderr. Non-UTF-8
        // bytes (never produced by the fmt layer) pass through unscrubbed rather than being lost.
        match std::str::from_utf8(buf) {
            Ok(text) => {
                let scrubbed = redact::scrub(text);
                std::io::stderr().write_all(scrubbed.as_bytes())?;
            }
            Err(_) => {
                std::io::stderr().write_all(buf)?;
            }
        }
        // Report the original length consumed (we wrote the whole logical line).
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ScrubMakeWriter {
    type Writer = ScrubWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ScrubWriter
    }
}

/// Initialise structured logging at the command boundary only. Idempotent: a second
/// call is a no-op (the global subscriber is already set). Reads `RUST_LOG`.
///
/// The writer is the t37 [`ScrubMakeWriter`]: a defense-in-depth scrub of every emitted line. The
/// PRIMARY secret-out-of-logs control is `qfs_secrets::Secret` (redacting `Debug`/`Display`, no
/// `Serialize`) — a secret cannot be formatted in the first place; this scrubber is the backup.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // `try_init` returns Err if a global subscriber is already set; ignore it so
    // repeated calls (e.g. in tests) do not panic.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(ScrubMakeWriter)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A no-op shell launcher for the dispatch tests (the real REPL is tested in the binary
    /// crate's `shell` module). Returns exit 0, standing in for an immediate EOF.
    fn noop_shell() -> i32 {
        0
    }

    /// An empty describe registry for the dispatch tests (the real describe-only drivers are
    /// wired + tested in the binary crate). With no driver registered, `qfs describe /x` resolves
    /// to a structured `unknown_mount` capability error (exit 3) — never a panic.
    fn empty_describe() -> qfs_core::MountRegistry {
        qfs_core::MountRegistry::new()
    }

    /// A stand-in skill provider for the dispatch tests (the real embedded skill is wired + tested
    /// in the binary crate). Returns a minimal loop-landmarked text so the `skill` arm is total.
    fn stub_skill(examples: bool) -> String {
        if examples {
            "DESCRIBE PREVIEW COMMIT\n## Example corpus\n".to_string()
        } else {
            "DESCRIBE PREVIEW COMMIT\n".to_string()
        }
    }

    /// Run with the no-op shell + serve launchers + empty describe + stub skill providers (every
    /// non-shell/serve/describe/skill test path ignores them).
    fn run_t<I, T>(args: I) -> i32
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        run(args, &noop_shell, &|_cfg| 0, &empty_describe, &stub_skill)
    }

    #[test]
    fn run_dispatch_resolves_single_statement_source() {
        // t29: `qfs run` now dispatches into the execution layer. Resolving exactly one
        // statement source is a usage gate; zero sources is exit 2 (usage).
        let engine = Engine::new();
        let code = dispatch_run(
            &engine,
            RunOpts {
                stmt: None,
                expr: None,
                format: Some("json".into()),
                json: true,
                commit: false,
                commit_irreversible: false,
                quiet: false,
            },
        );
        assert_eq!(code, 2, "no statement source is a usage error (exit 2)");
    }

    #[test]
    fn no_subcommand_invokes_the_shell_launcher() {
        // The shell is now implemented (t28) and launched via the injected ShellLauncher: with
        // no subcommand, `run` calls the launcher and returns its exit code. The real REPL +
        // local read facet are tested in the binary crate's `shell` module.
        let launched = std::cell::Cell::new(false);
        let code = run(
            ["qfs"],
            &|| {
                launched.set(true);
                0
            },
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
        );
        assert!(
            launched.get(),
            "no subcommand must invoke the shell launcher"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn run_bad_syntax_is_parse_error_exit_two() {
        // `qfs run -e 'anything'` reaches a structured parse error (exit 2), not a panic.
        let code = run_t(["qfs", "run", "-e", "anything"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn run_relative_path_is_usage_error_exit_two() {
        // A relative-path address in one-shot mode is rejected with a usage error (exit 2).
        let code = run_t(["qfs", "run", "-e", "FROM mail/inbox |> LIMIT 1"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn run_unknown_source_is_capability_exit_three() {
        // With no read driver registered, an absolute `FROM /x` resolves to a structured
        // capability error (exit 3) — never a panic.
        let code = run_t(["qfs", "run", "-e", "FROM /mail/inbox |> LIMIT 1", "--json"]);
        assert_eq!(code, 3);
    }

    #[test]
    fn serve_dispatches_through_the_injected_launcher() {
        // t32: `qfs serve <config>` is dispatched through the injected ServeLauncher (the
        // binary composition root that wires the HTTP binding). qfs-cmd only routes to it with
        // the config path and returns its exit code — here a noop launcher returning 0.
        let launched = std::cell::Cell::new(false);
        let code = run(
            ["qfs", "serve", "x.qfs"],
            &noop_shell,
            &|cfg| {
                launched.set(cfg.ends_with("x.qfs"));
                0
            },
            &empty_describe,
            &stub_skill,
        );
        assert!(
            launched.get(),
            "serve must invoke the serve launcher with the config path"
        );
        assert_eq!(code, 0);
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
        // `qfs account list` parses cleanly and reaches the structured stub (exit 1).
        assert_eq!(run_t(["qfs", "account", "list"]), 1);
        assert_eq!(run_t(["qfs", "account", "add", "mail", "work"]), 1);
    }

    #[test]
    fn help_exits_zero_without_panic() {
        let code = run_t(["qfs", "--help"]);
        assert_eq!(code, 0);
    }

    #[test]
    fn skill_subcommand_dispatches_to_the_provider_and_exits_zero() {
        // `qfs skill` (and `qfs skill --examples`) route to the injected SkillProvider and exit 0.
        // The real embedded SKILL.md is wired + content-checked in the binary crate; here we only
        // assert the dispatch + flag plumbing through clap.
        let saw_examples = std::cell::Cell::new(false);
        let provider = |examples: bool| {
            saw_examples.set(examples);
            "DESCRIBE PREVIEW COMMIT\n".to_string()
        };
        assert_eq!(
            run(
                ["qfs", "skill"],
                &noop_shell,
                &|_| 0,
                &empty_describe,
                &provider
            ),
            0
        );
        assert!(!saw_examples.get(), "`qfs skill` passes examples=false");
        assert_eq!(
            run(
                ["qfs", "skill", "--examples"],
                &noop_shell,
                &|_| 0,
                &empty_describe,
                &provider
            ),
            0
        );
        assert!(
            saw_examples.get(),
            "`qfs skill --examples` passes examples=true"
        );
    }

    #[test]
    fn run_help_snapshot_pins_the_oneshot_surface() {
        // Render `qfs run --help` and assert the stable t29 contract surface is present. A
        // brittle full-text snapshot is avoided; instead pin the load-bearing flags/args an
        // agent scripts against, so a rename/removal fails CI.
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("run")
            .expect("run subcommand exists")
            .render_long_help()
            .to_string();
        for needle in [
            "[STMT]", "--expr", "--format", "--commit", "--quiet", "stdin", "PREVIEW",
        ] {
            assert!(
                help.contains(needle),
                "`qfs run --help` lost the stable surface `{needle}`:\n{help}"
            );
        }
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
