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

/// The injected **job launcher** (t65, decision M revised): the binary supplies `qfs job <verb>`.
/// **qfs is not a scheduler** — a `CREATE JOB … EVERY … DO …` row is a *saved named plan + its
/// intended cadence* that an EXTERNAL scheduler (OS `cron` / Cloudflare Cron Triggers) invokes.
/// `qfs job run <config> <name>` builds + commits that saved plan once through the SAME policy gate
/// and IrreversibleGuard the CLI one-shot uses; `qfs job cron <config> <name>` emits the crontab
/// line for the host crontab. The whole boot→rehydrate→build→gate→apply path lives in the binary
/// composition root (it owns `qfs-host` / `qfs-exec` / `qfs-runtime`), NOT in qfs-cmd (which must
/// stay off them) — the [`ServeLauncher`] pattern. qfs-cmd only parses the verb and forwards the
/// [`JobRequest`], returning the launcher's process exit code.
pub type JobLauncher<'a> = dyn Fn(&JobRequest) -> i32 + 'a;

/// Which `qfs job` action the binary launcher performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    /// `qfs job run` — invoke a saved JOB's plan once (the external-scheduler entrypoint).
    Run,
    /// `qfs job cron` — emit the OS-cron crontab line that invokes the JOB on its cadence.
    Cron,
}

/// An owned `qfs job <verb>` request the binary-injected [`JobLauncher`] executes. The config path
/// and job name are safe metadata; no credential is ever carried here (the commit resolves creds
/// the same way `qfs run --commit` does — from the env / connection store, never argv).
#[derive(Debug, Clone)]
pub struct JobRequest {
    /// The action (`run` / `cron`).
    pub action: JobAction,
    /// The `.qfs` config that defines the JOB (the saved-plan source).
    pub config: PathBuf,
    /// The JOB name (the `/server/jobs` row key) to run / emit a crontab line for.
    pub name: String,
    /// Apply the plan (`run` only; PREVIEW by default, mirroring `qfs run`).
    pub commit: bool,
    /// Acknowledge an irreversible effect in this unattended run (`run` only) — required for a
    /// REMOVE / declared-irreversible CALL, fail-closed without it (the same floor as `qfs run`).
    pub commit_irreversible: bool,
    /// Global `--json` flag (output mode).
    pub json: bool,
    /// `--format json|table` (`run` only).
    pub format: Option<String>,
    /// `--quiet` (`run` only): suppress the success receipt; never the error body.
    pub quiet: bool,
}

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

/// A parsed `qfs connection <verb>` request, handed to the binary-injected [`ConnectionLauncher`]. The
/// credential value itself is **never** carried here (it would leak into argv / history / `ps`);
/// the launcher reads it from stdin/prompt. Driver + connection selectors are safe metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionAction {
    /// `connection add <driver> <connection>` — store (or replace) a credential.
    Add { driver: String, connection: String },
    /// `connection list [driver]` — list configured connections (metadata only).
    List { driver: Option<String> },
    /// `connection use <driver> <connection>` — set the persistent active connection.
    Use { driver: String, connection: String },
    /// `connection remove <driver> <connection>` — delete (idempotent).
    Remove { driver: String, connection: String },
    /// `connection rotate <driver> <connection>` — re-mint the secret (read from stdin) + clear
    /// revocation (t79). The value is NEVER carried here; the launcher reads it from stdin.
    Rotate { driver: String, connection: String },
    /// `connection revoke <driver> <connection>` — mark the connection unresolvable (t79).
    Revoke { driver: String, connection: String },
    /// `connection rekey` — re-wrap the store's data-key under a new passphrase (t79). The new
    /// passphrase is NEVER carried here; the launcher reads it from stdin (old = `QFS_PASSPHRASE`).
    Rekey,
    /// `connection import-env` — print the `CREATE CONNECTION` declarations equivalent to the
    /// current `QFS_SQL_*` / `QFS_GIT_*` env vars (the migration off the deprecated convention).
    ImportEnv,
    /// `connect <path> …` — bind a defined PATH to a driver + credential (a full connect), or to
    /// an existing path (an alias). The direct-DB-I/O twin of the `CONNECT` statement (EPIC
    /// 20260701100000 / t100020); writes the Project DB `path_binding` table (no passphrase — the
    /// `secret_ref` is a REFERENCE, never a value). Exactly one of `driver` / `alias_of` is set.
    Connect {
        /// The user-defined path (the mount point), e.g. `/work/orders`.
        path: String,
        /// The driver id for a full connect (mutually exclusive with `alias_of`).
        driver: Option<String>,
        /// The non-secret `AT` locator (full connect only).
        at: Option<String>,
        /// The secret REFERENCE (`env:VAR` / `vault:driver/connection`) — never a value.
        secret_ref: Option<String>,
        /// The target defined path for an ALIAS (mutually exclusive with `driver`).
        alias_of: Option<String>,
        /// Which qfs host owns the mount (ADR 0008 §1); `None` = the implicit `local` host.
        host: Option<String>,
        /// The service-account LABEL the mount binds (ADR 0008 §4 — the mount carries the
        /// account, e.g. a Google email). A selector, never a token.
        account: Option<String>,
    },
    /// `disconnect <path>` — remove a defined path (idempotent; aliases cascade). The direct-DB-I/O
    /// twin of the `DISCONNECT` statement (t100020).
    Disconnect {
        /// The user-defined path to remove.
        path: String,
    },
    /// `connection paths` — list the defined-path bindings (the `path_binding` registry): metadata
    /// only (path, driver, alias target, secret REFERENCE), never a secret value (t100020).
    ListPaths,
}

/// The injected **connection launcher**: the binary supplies the credential-store I/O (it depends on
/// `qfs-secrets`'s encrypted `LocalStore`, which `qfs-cmd` may not — the dep_direction guard keeps
/// `qfs-cmd` off the concrete backends). `qfs-cmd` only parses the verb and calls this, exactly
/// like the shell / serve / describe launchers. Returns the process exit code.
pub type ConnectionLauncher<'a> = dyn Fn(&ConnectionAction) -> i32 + 'a;

/// A parsed `qfs identity <verb>` request, handed to the binary-injected [`IdentityLauncher`] (t45).
/// This is the AUTHENTICATION surface — local sign-up + a session-less `whoami` (decision §4.1:
/// identity is not authorization; sessions land in t46). Like [`ConnectionAction`], the **password is
/// never carried here** (it would leak into argv / shell history / `ps`); the launcher reads it from
/// stdin. The email is a handle (safe metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityAction {
    /// `identity whoami [email]` — print a user's email + id (NEVER the password hash). With no
    /// email and no session yet (t46), it resolves the sole user if the deployment has exactly one.
    /// (Signing up moved to `qfs init` — ADR 0008 §2: no unverified password on the CLI.)
    Whoami { email: Option<String> },
}

/// The injected **identity launcher** (t45): the binary supplies the System-DB-backed identity store
/// I/O (it depends on `qfs-store` + `qfs-identity`, which `qfs-cmd` may not — the dep_direction guard
/// keeps `qfs-cmd` off the concrete backends). `qfs-cmd` only parses the verb and calls this, exactly
/// like the connection launcher. Returns the process exit code.
pub type IdentityLauncher<'a> = dyn Fn(&IdentityAction) -> i32 + 'a;

/// A parsed `qfs init` request, handed to the binary-injected [`InitLauncher`] (ADR 0008 §2 —
/// the first-run wizard). Carries the optional operator email only (an accountability label);
/// NO password exists on this surface at all — local auth is the OS login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitAction {
    /// The operator email; `None` means prompt on a terminal (or fail non-interactively).
    pub email: Option<String>,
}

/// The injected **init launcher** (ADR 0008 §2): the binary supplies the identity-store + vault
/// I/O (`qfs-store`/`qfs-identity`/`qfs-secrets`, which `qfs-cmd` may not depend on). Returns the
/// process exit code.
pub type InitLauncher<'a> = dyn Fn(&InitAction) -> i32 + 'a;

/// A parsed `qfs vault <verb>` request, handed to the binary-injected [`VaultLauncher`]
/// (ADR 0008 §5 — KeyGuardian). Selectors + metadata only: no passphrase, no KEK, no wrap bytes
/// ever ride here — the launcher owns every byte of key material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultAction {
    /// `vault slots` — list the vault-key slots (id, guardian kind, created_at). Metadata only.
    Slots,
    /// `vault enroll <guardian>` — wrap the store's data-key under one more guardian (today:
    /// `keychain`, the platform secret service). The KEK is minted and stored by the launcher.
    Enroll {
        /// The guardian kind to enroll (e.g. `keychain`).
        guardian: String,
    },
    /// `vault revoke <slot>` — delete one vault-key wrap (the last slot is refused).
    Revoke {
        /// The slot id to revoke (see `vault slots`).
        slot_id: i64,
    },
}

/// The injected **vault launcher** (ADR 0008 §5): the binary supplies the slot I/O + the guardian
/// backends (the envelope store and the OS keyring, which `qfs-cmd` may not depend on — the
/// dep_direction guard). `qfs-cmd` only parses the verb and calls this. Returns the exit code.
pub type VaultLauncher<'a> = dyn Fn(&VaultAction) -> i32 + 'a;

/// A parsed `qfs invite <verb>` request, handed to the binary-injected [`InviteLauncher`] (t55, M5).
/// This is the team-membership front door — a host operator MINTS a one-time, expiring invite, the
/// invitee REDEEMS it to create their local identity + a membership (identity ≠ authorization, §4.1).
/// Like [`IdentityAction`], the **password is never carried here** (the launcher reads it from STDIN
/// at redeem). The redeem `token` IS carried (it is the one-time-URL secret the invitee presents) —
/// it is single-use, burned on redeem, and never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteAction {
    /// `invite create [--email <e>] [--scope host|project] [--project <p>] [--role <r>] [--ttl <s>]`
    /// — mint an invite and print its one-time URL/token EXACTLY once. Metadata only (no secret on
    /// argv); the token is generated + returned by the launcher.
    Create {
        /// The optional invitee email (the delivery target when mail is configured — a seam).
        email: Option<String>,
        /// The membership scope to seed (`host` default, or `project`).
        scope: Option<String>,
        /// The project ref for a project-scoped invite.
        project: Option<String>,
        /// The initial membership role label (`member` default).
        role: Option<String>,
        /// The invite time-to-live in seconds (the launcher applies a default if absent).
        ttl_secs: Option<i64>,
    },
    /// `invite redeem <token> <email>` — redeem the one-time token to create the local user +
    /// membership. The password is read from STDIN by the launcher, never argv.
    Redeem {
        /// The one-time token off the invite URL (single-use; burned on redeem).
        token: String,
        /// The email the redeemer signs up with (the new user's handle + local account subject).
        email: String,
    },
    /// `invite revoke <id>` — revoke a still-pending invite so its token can no longer redeem.
    Revoke {
        /// The invite id to revoke.
        id: i64,
    },
}

/// The injected **invite launcher** (t55): the binary supplies the System-DB-backed invite store I/O
/// and the CSPRNG that mints the one-time token (it depends on `qfs-store` + `qfs-identity`, which
/// `qfs-cmd` may not). `qfs-cmd` only parses the verb and calls this, exactly like the identity
/// launcher. Returns the process exit code.
pub type InviteLauncher<'a> = dyn Fn(&InviteAction) -> i32 + 'a;

/// The injected **run-context provider**: the binary supplies the
/// `(Engine, ReadRegistry, SafetyMode)` for `qfs run` — the [`Engine`] whose mount registry has the
/// real drivers (so a `/path …` source resolves + plans + pushes down), the
/// [`qfs_exec::ReadRegistry`] of `ReadDriver` scan facets that execute the read, and the resolved
/// selectable **safety mode** (t59) that governs the one-shot commit gate (the deployment setting
/// from `/sys/settings`, falling back to the safe default). All live in the binary (which owns the
/// runtime-coupled local adapter + the System DB) — NOT in qfs-cmd, which stays off qfs-driver-local.
/// Mirrors the describe / shell / commit injections.
pub type RunContextProvider<'a> =
    dyn Fn() -> (Engine, qfs_exec::ReadRegistry, qfs_core::SafetyMode) + 'a;

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

    /// Disable ANSI color in human output. Color is on only when writing to a terminal and is also
    /// suppressed by the standard `NO_COLOR` environment variable.
    #[arg(long = "no-color", global = true)]
    no_color: bool,

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
        /// The statement to execute positionally, e.g. `/mail/inbox |> SELECT subject`.
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
    /// Manage stored credentials per driver/connection (t27, RFD-0001 §10). The connection
    /// *name* is metadata (safe to print); the credential itself is never echoed.
    Connection {
        #[command(subcommand)]
        verb: ConnectionVerb,
    },
    /// Bind a defined PATH to a driver + credential — a "defined path" that mounts a connection
    /// (EPIC 20260701100000 / t100020). The CLI twin of the `CONNECT` statement; writes the Project
    /// DB `path_binding` registry (the single source of truth — no `connections.qfs` file).
    ///
    /// A full connect names a `--driver` (with optional `--at` locator + `--secret` REFERENCE);
    /// an alias names `--alias-of <existing-path>` instead. The secret is a REFERENCE
    /// (`env:VAR` / `vault:driver/connection`), never a value — nothing secret rides in argv.
    Connect {
        /// The user-defined path (the mount point), e.g. `/work/orders`.
        path: String,
        /// The driver id for a full connect (e.g. `postgres`). Mutually exclusive with `--alias-of`.
        #[arg(long = "driver", value_name = "DRIVER")]
        driver: Option<String>,
        /// The non-secret `AT` locator (full connect only), e.g. `postgres://db/orders`.
        #[arg(long = "at", value_name = "LOCATOR")]
        at: Option<String>,
        /// The secret REFERENCE (`env:VAR` / `vault:driver/connection`) — never a value.
        #[arg(long = "secret", value_name = "REF")]
        secret: Option<String>,
        /// Bind as an ALIAS of this existing defined path. Mutually exclusive with `--driver`.
        #[arg(long = "alias-of", value_name = "PATH")]
        alias_of: Option<String>,
        /// Which qfs host owns the mount (ADR 0008); omitted = the implicit `local` host.
        #[arg(long = "host", value_name = "HOST")]
        host: Option<String>,
        /// The service-account label the mount binds (e.g. a Google email) — never a token.
        #[arg(long = "account", value_name = "LABEL")]
        account: Option<String>,
    },
    /// Remove a defined path (idempotent; aliases cascade). The CLI twin of `DISCONNECT` (t100020).
    Disconnect {
        /// The user-defined path to remove, e.g. `/work/orders`.
        path: String,
    },
    /// Ready this machine (ADR 0008 — the first-run wizard): create the encrypted credential
    /// store (choosing its passphrase) and register the operator identity — one operator per OS
    /// user, no password (your OS login is the authentication; the email is an accountability
    /// label). Idempotent: re-running reports what exists.
    Init {
        /// The operator email. Omit it on a terminal to be prompted.
        email: Option<String>,
    },
    /// Manage the vault's key slots (ADR 0008 — KeyGuardian): list them, enroll the OS keychain
    /// so this host unlocks without a passphrase, or revoke a slot. The passphrase slot is
    /// enrolled automatically when the store is first created.
    Vault {
        #[command(subcommand)]
        verb: VaultVerb,
    },
    /// Manage local identity: sign up (email + password) and look yourself up (t45, roadmap M1).
    ///
    /// AUTHENTICATION ONLY (decision §4.1: identity is not authorization). A signed-up user can do
    /// nothing privileged yet — there is local sign-up, **no session** (sessions land in t46, real
    /// auth in M2). The password is read from STDIN (never argv); the password hash is never printed.
    Identity {
        #[command(subcommand)]
        verb: IdentityVerb,
    },
    /// Manage team invites + membership (t55, roadmap M5). An operator mints a one-time, expiring
    /// invite; the invitee redeems it to create their local identity and join the host.
    ///
    /// MEMBERSHIP, not authorization (§4.1): redeeming makes someone a *member*, never grants a
    /// capability (the ACL is t57). The one-time token is minted by a CSPRNG, returned ONCE, and
    /// stored only as a hash; redeem is single-use and expiring. Email delivery is a documented seam
    /// — when mail is not configured, the printed one-time URL is the artifact.
    Invite {
        #[command(subcommand)]
        verb: InviteVerb,
    },
    /// Run / schedule a saved JOB — the invokable unit an EXTERNAL scheduler drives (t65, decision
    /// M revised). **qfs is not a scheduler**: a `CREATE JOB … EVERY … DO …` row is a saved named
    /// plan + its intended cadence, not something qfs fires itself. OS `cron` (individual) and
    /// Cloudflare Cron Triggers (managed) own the *when*; qfs supplies the safe *what*.
    Job {
        #[command(subcommand)]
        verb: JobVerb,
    },
    // The absence of a subcommand starts the interactive shell (handled in `run`).
}

/// `qfs job <verb>` — the saved-JOB invocation verbs (t65). Maps onto the injected [`JobLauncher`]
/// over the booted config's `/server/jobs` rows. The internal scheduler daemon is RETIRED; these
/// verbs are how an external scheduler (or a human) drives a defined job.
#[derive(Subcommand, Debug)]
enum JobVerb {
    /// Run a saved JOB's plan once — the entrypoint an external scheduler's crontab line invokes.
    ///
    /// Loads the named `/server/jobs` plan from `config`, rehydrates it, and (with `--commit`)
    /// applies it through the SAME policy gate + IrreversibleGuard the CLI one-shot uses. PREVIEW
    /// by default (no apply). Non-interactive + exit-code-correct, suitable for a crontab line:
    /// `0 * * * *  qfs job run /etc/qfs/app.qfs nightly --commit` (ensure `QFS_PASSPHRASE` + any
    /// connection creds are in cron's environment).
    Run {
        /// The `.qfs` config that defines the JOB.
        config: PathBuf,
        /// The JOB name (the `/server/jobs` row key).
        name: String,
        /// Apply the plan (default is PREVIEW), mirroring `qfs run --commit`.
        #[arg(long = "commit")]
        commit: bool,
        /// Acknowledge applying an irreversible effect (a `REMOVE` / `CALL`) in this unattended
        /// run. Without it, a `--commit` of an irreversible plan fails closed (the same floor as
        /// `qfs run --commit-irreversible`): an external trigger has no TTY to confirm on.
        #[arg(long = "commit-irreversible")]
        commit_irreversible: bool,
        /// Output format: `json` or `table`. Default: `table` on a TTY, `json` when piped.
        #[arg(long = "format", value_name = "FORMAT")]
        format: Option<String>,
        /// Suppress the success receipt; never suppresses the error body.
        #[arg(long = "quiet", short = 'q')]
        quiet: bool,
    },
    /// Emit the OS-cron crontab line that invokes this JOB on its `EVERY` cadence — the individual
    /// counterpart of the `[triggers] crons` entry the managed (Cloudflare) wrangler generation
    /// emits. Drop the printed line into a host crontab; qfs runs no scheduler of its own.
    Cron {
        /// The `.qfs` config that defines the JOB.
        config: PathBuf,
        /// The JOB name (the `/server/jobs` row key).
        name: String,
    },
}

/// `qfs connection <verb>` — the credential-store management verbs (t27). Each maps onto a
/// [`qfs_core::Secrets`] backend + the resolution model; the credential value is read
/// from a prompt / stdin (never an argv, which would leak into shell history and `ps`).
#[derive(Subcommand, Debug)]
enum ConnectionVerb {
    /// Add (or replace) the credential for a driver's named connection.
    Add {
        /// The driver this connection belongs to, e.g. `mail`, `s3`.
        driver: String,
        /// The connection name, e.g. `work`, `personal`.
        connection: String,
    },
    /// List configured connections (optionally for one driver). Prints selectors + metadata
    /// only — never a credential.
    List {
        /// Restrict the listing to one driver.
        driver: Option<String>,
    },
    /// Set the persistent active connection for a driver (`connection use`).
    Use {
        /// The driver to set the active connection for.
        driver: String,
        /// The connection to make active.
        connection: String,
    },
    /// Remove the credential for a driver's named connection (idempotent).
    Remove {
        /// The driver.
        driver: String,
        /// The connection to remove.
        connection: String,
    },
    /// Rotate (re-mint) the credential for a driver's named connection (t79): read a NEW secret from
    /// stdin, re-seal it, and clear any revocation. The offboarding answer — replace, not un-grant.
    Rotate {
        /// The driver this connection belongs to.
        driver: String,
        /// The connection to re-mint.
        connection: String,
    },
    /// Revoke a driver's named connection (t79): mark it unresolvable so a later bind fails closed
    /// and the secret is never returned (offboarding / compromise). Other connections keep working.
    Revoke {
        /// The driver this connection belongs to.
        driver: String,
        /// The connection to revoke.
        connection: String,
    },
    /// Re-wrap the credential store's data-key under a NEW passphrase (t79): read the new passphrase
    /// from stdin; the current `QFS_PASSPHRASE` is the old one. Existing secrets stay decryptable; the
    /// old passphrase stops unlocking. One re-wrap, never an N-way re-encryption of every secret.
    Rekey,
    /// Print the `CREATE CONNECTION` declarations equivalent to the current `QFS_SQL_*` / `QFS_GIT_*`
    /// env vars — the one-command migration off the deprecated env-var alias convention. Reads no
    /// secret; writes the declarations to stdout for you to paste into a `connections.qfs`.
    #[command(name = "import-env")]
    ImportEnv,
    /// List the defined-path bindings (the `path_binding` registry, t100020): metadata only —
    /// the path, its driver / alias target, and the secret REFERENCE, never a secret value.
    Paths,
}

/// `qfs vault <verb>` — the KeyGuardian slot verbs (ADR 0008 §5). Maps onto the injected
/// [`VaultLauncher`]; no key material ever parses out of argv.
#[derive(Subcommand, Debug)]
enum VaultVerb {
    /// List the vault-key slots: id, guardian kind, created_at (metadata only, never key bytes).
    Slots,
    /// Enroll a new guardian slot — today `keychain` (the platform secret service), so this host
    /// unlocks the credential store without a passphrase from then on.
    Enroll {
        /// The guardian kind to enroll (`keychain`).
        guardian: String,
    },
    /// Revoke a vault-key slot by id (the last remaining slot is refused).
    Revoke {
        /// The slot id (see `qfs vault slots`).
        slot_id: i64,
    },
}

/// `qfs identity <verb>` — the local-identity verbs (t45). Maps onto the injected
/// [`IdentityLauncher`] over the System-DB identity store. The password is read from STDIN (never an
/// argv, which would leak into shell history and `ps`); the password hash is never printed.
#[derive(Subcommand, Debug)]
enum IdentityVerb {
    /// Print a user's email + id (NEVER the password hash). With an `email`, looks that user up;
    /// with none and exactly one user on this host, prints it (there is no session yet — t46).
    /// (Signing up moved to `qfs init` — ADR 0008.)
    Whoami {
        /// The user to look up. Optional: omit it to resolve the sole user.
        email: Option<String>,
    },
}

/// `qfs invite <verb>` — the team-invite + membership verbs (t55). Maps onto the injected
/// [`InviteLauncher`] over the System-DB invite store. The one-time token is minted by the launcher's
/// CSPRNG and printed once at `create`; the redeem password is read from STDIN (never argv).
#[derive(Subcommand, Debug)]
enum InviteVerb {
    /// Mint a one-time, expiring invite and print its URL/token EXACTLY once (store it now — it is
    /// never shown again). Metadata only; no secret on argv.
    Create {
        /// The optional invitee email (the delivery target when mail is configured — a seam).
        #[arg(long)]
        email: Option<String>,
        /// The membership scope to seed: `host` (default) or `project`.
        #[arg(long)]
        scope: Option<String>,
        /// The project ref for a `--scope project` invite.
        #[arg(long)]
        project: Option<String>,
        /// The initial membership role label (`member` by default — a label, not a grant; §4.1).
        #[arg(long)]
        role: Option<String>,
        /// The invite lifetime in seconds (a default is applied if omitted).
        #[arg(long = "ttl")]
        ttl_secs: Option<i64>,
    },
    /// Redeem a one-time invite token to create the local user + membership. The password is read
    /// from STDIN (e.g. `printf %s "$PW" | qfs invite redeem <token> a@b.com`), never argv.
    Redeem {
        /// The one-time token off the invite URL (single-use; burned on redeem).
        token: String,
        /// The email to sign up with (the new user's handle + local account subject).
        email: String,
    },
    /// Revoke a still-pending invite so its token can no longer redeem (idempotent).
    Revoke {
        /// The invite id to revoke.
        id: i64,
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
// The binary's single composition-root entrypoint: each argument is a distinct injected seam
// (shell / serve / describe / skill / connection / identity / invite / job / commit-applier /
// run-context) the leaf binary supplies so qfs-cmd stays off the concrete driver/runtime/secrets
// crates. The count is the surface of that injection, not incidental coupling.
#[allow(clippy::too_many_arguments)]
pub fn run<I, T>(
    args: I,
    shell: &ShellLauncher,
    serve: &ServeLauncher,
    describe: &DescribeProvider,
    skill: &SkillProvider,
    connection: &ConnectionLauncher,
    identity: &IdentityLauncher,
    init: &InitLauncher,
    vault: &VaultLauncher,
    invite: &InviteLauncher,
    job: &JobLauncher,
    apply: &qfs_exec::WorldApply,
    run_ctx: &RunContextProvider,
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

    // Resolve human-output color ONCE, process-wide, before any rendering: on only for a real
    // terminal with `NO_COLOR` unset, no `--no-color`, and not `--json` (JSON never colorizes). The
    // renderers (table headers, the preview's irreversible marker, error lines) consult this global.
    let color = {
        use std::io::IsTerminal;
        !cli.no_color
            && !cli.json
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal()
    };
    qfs_core::color::set_enabled(color);

    let output = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };

    // The Session carries the resolved output mode. (The run Engine is no longer built here — the
    // run path builds it from the injected RunContextProvider, which carries the real drivers.)
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
            RunOpts {
                stmt: stmt.clone(),
                expr: expr.clone(),
                format: format.clone(),
                json: cli.json,
                commit: *commit,
                commit_irreversible: *commit_irreversible,
                quiet: *quiet,
            },
            apply,
            run_ctx,
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
        // `connection` is dispatched through the injected launcher (the binary owns the encrypted
        // credential store; qfs-cmd stays off the concrete backend). Returns the exit code directly.
        Some(Command::Connection { verb }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch connection via launcher");
            return connection(&connection_action(&verb));
        }
        // `connect` / `disconnect` (t100020): the CLI twin of the CONNECT/DISCONNECT statements —
        // dispatched through the SAME connection launcher (the binary owns the Project-DB binding
        // I/O). Returns the exit code directly.
        Some(Command::Connect {
            path,
            driver,
            at,
            secret,
            alias_of,
            host,
            account,
        }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch connect via launcher");
            return connection(&ConnectionAction::Connect {
                path,
                driver,
                at,
                secret_ref: secret,
                alias_of,
                host,
                account,
            });
        }
        Some(Command::Disconnect { path }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch disconnect via launcher");
            return connection(&ConnectionAction::Disconnect { path });
        }
        // `identity` is dispatched through the injected launcher (the binary owns the System-DB
        // identity store; qfs-cmd stays off the concrete backend). Returns the exit code directly.
        Some(Command::Identity { verb }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch identity via launcher");
            return identity(&identity_action(&verb));
        }
        // `init` (ADR 0008 §2) is dispatched through the injected launcher (the binary owns the
        // identity-store + vault I/O; qfs-cmd stays off the concrete backends). Returns the code.
        Some(Command::Init { email }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch init via launcher");
            return init(&InitAction { email });
        }
        // `vault` (ADR 0008 §5) is dispatched through the injected launcher (the binary owns the
        // slot I/O + the OS-keyring guardian; qfs-cmd stays off both). Returns the exit code.
        Some(Command::Vault { verb }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch vault via launcher");
            return vault(&vault_action(&verb));
        }
        // `invite` is dispatched through the injected launcher (the binary owns the System-DB invite
        // store + the token CSPRNG; qfs-cmd stays off the concrete backend). Returns the exit code.
        Some(Command::Invite { verb }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch invite via launcher");
            return invite(&invite_action(&verb));
        }
        // `job` is dispatched through the injected launcher (the binary owns the boot→rehydrate→
        // build→policy-gate→IrreversibleGuard→apply path over qfs-host/qfs-exec/qfs-runtime;
        // qfs-cmd stays off them). The internal scheduler daemon is RETIRED — this is how an
        // external scheduler drives a defined job. Returns the exit code directly.
        Some(Command::Job { verb }) => {
            tracing::debug!(target: "qfs::cmd", "dispatch job via launcher");
            return job(&job_request(&verb, cli.json));
        }
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
fn dispatch_run(opts: RunOpts, apply: &qfs_exec::WorldApply, run_ctx: &RunContextProvider) -> i32 {
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

    // The run context: the binary supplies the Engine (mounts with the real drivers, so a `FROM`
    // source resolves + plans + pushes down) and the ReadRegistry (the scan facets). With no
    // driver for a mount, a `/x` resolves to a structured capability error (exit 3).
    let (engine, reads, safety_mode) = run_ctx();
    let ctx = qfs_exec::ExecCtx {
        engine: &engine,
        reads: &reads,
        // The binary injects the real interpreter-backed commit; qfs-cmd stays off qfs-runtime.
        world_apply: Some(apply),
        // The resolved selectable safety mode (t59) governs the one-shot commit gate.
        safety_mode,
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

/// Map the clap-parsed [`ConnectionVerb`] to the public [`ConnectionAction`] handed to the injected
/// [`ConnectionLauncher`]. Pure (selectors only); the credential value is never carried — the
/// launcher reads it from stdin/prompt, never from argv (which would leak into history / `ps`).
fn connection_action(verb: &ConnectionVerb) -> ConnectionAction {
    match verb {
        ConnectionVerb::Add { driver, connection } => ConnectionAction::Add {
            driver: driver.clone(),
            connection: connection.clone(),
        },
        ConnectionVerb::List { driver } => ConnectionAction::List {
            driver: driver.clone(),
        },
        ConnectionVerb::Use { driver, connection } => ConnectionAction::Use {
            driver: driver.clone(),
            connection: connection.clone(),
        },
        ConnectionVerb::Remove { driver, connection } => ConnectionAction::Remove {
            driver: driver.clone(),
            connection: connection.clone(),
        },
        ConnectionVerb::Rotate { driver, connection } => ConnectionAction::Rotate {
            driver: driver.clone(),
            connection: connection.clone(),
        },
        ConnectionVerb::Revoke { driver, connection } => ConnectionAction::Revoke {
            driver: driver.clone(),
            connection: connection.clone(),
        },
        ConnectionVerb::Rekey => ConnectionAction::Rekey,
        ConnectionVerb::ImportEnv => ConnectionAction::ImportEnv,
        ConnectionVerb::Paths => ConnectionAction::ListPaths,
    }
}

/// Map the clap-parsed [`IdentityVerb`] to the public [`IdentityAction`] handed to the injected
/// [`IdentityLauncher`]. Pure (handles only); the password is never carried — the launcher reads it
/// from STDIN, never from argv (which would leak into history / `ps`).
/// Map the clap-parsed [`VaultVerb`] to the public [`VaultAction`] handed to the injected
/// [`VaultLauncher`]. Pure (selectors only); key material never parses out of argv.
fn vault_action(verb: &VaultVerb) -> VaultAction {
    match verb {
        VaultVerb::Slots => VaultAction::Slots,
        VaultVerb::Enroll { guardian } => VaultAction::Enroll {
            guardian: guardian.clone(),
        },
        VaultVerb::Revoke { slot_id } => VaultAction::Revoke { slot_id: *slot_id },
    }
}

fn identity_action(verb: &IdentityVerb) -> IdentityAction {
    match verb {
        IdentityVerb::Whoami { email } => IdentityAction::Whoami {
            email: email.clone(),
        },
    }
}

/// Map the clap-parsed [`InviteVerb`] to the public [`InviteAction`] handed to the injected
/// [`InviteLauncher`]. Pure (selectors/handles only); the password is never carried — the launcher
/// reads it from STDIN at redeem. The redeem token IS carried (it is the one-time-URL secret the
/// invitee presents), single-use and never logged.
fn invite_action(verb: &InviteVerb) -> InviteAction {
    match verb {
        InviteVerb::Create {
            email,
            scope,
            project,
            role,
            ttl_secs,
        } => InviteAction::Create {
            email: email.clone(),
            scope: scope.clone(),
            project: project.clone(),
            role: role.clone(),
            ttl_secs: *ttl_secs,
        },
        InviteVerb::Redeem { token, email } => InviteAction::Redeem {
            token: token.clone(),
            email: email.clone(),
        },
        InviteVerb::Revoke { id } => InviteAction::Revoke { id: *id },
    }
}

/// Map a parsed `qfs job <verb>` into the owned [`JobRequest`] the binary launcher executes (t65).
/// Pure metadata transform — no boot, no I/O (the launcher owns those).
fn job_request(verb: &JobVerb, json: bool) -> JobRequest {
    match verb {
        JobVerb::Run {
            config,
            name,
            commit,
            commit_irreversible,
            format,
            quiet,
        } => JobRequest {
            action: JobAction::Run,
            config: config.clone(),
            name: name.clone(),
            commit: *commit,
            commit_irreversible: *commit_irreversible,
            json,
            format: format.clone(),
            quiet: *quiet,
        },
        JobVerb::Cron { config, name } => JobRequest {
            action: JobAction::Cron,
            config: config.clone(),
            name: name.clone(),
            commit: false,
            commit_irreversible: false,
            json,
            format: None,
            quiet: false,
        },
    }
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

    /// A stub connection launcher returning a sentinel exit code, so a test can assert the `connection`
    /// arm dispatched into the injected launcher (the real store I/O lives in the binary crate).
    fn stub_connection(_action: &ConnectionAction) -> i32 {
        7
    }

    /// A stub identity launcher returning a distinct sentinel, so a test can assert the `identity`
    /// arm dispatched into the injected launcher (the real System-DB store I/O lives in the binary).
    fn stub_identity(_action: &IdentityAction) -> i32 {
        9
    }

    /// A stub invite launcher returning a distinct sentinel, so a test can assert the `invite` arm
    /// dispatched into the injected launcher (the real System-DB invite store I/O lives in the binary).
    fn stub_invite(_action: &InviteAction) -> i32 {
        11
    }

    /// A stub vault launcher returning a distinct sentinel, so a test can assert the `vault` arm
    /// dispatched into the injected launcher (the real slot I/O + keyring guardian live in the
    /// binary crate).
    fn stub_vault(_action: &VaultAction) -> i32 {
        13
    }

    /// A stub init launcher returning a distinct sentinel, so a test can assert the `init` arm
    /// dispatched into the injected launcher (the real identity + vault I/O live in the binary).
    fn stub_init(_action: &InitAction) -> i32 {
        14
    }

    /// A stub job launcher: echoes a fixed code (the real boot→build→gate→apply path lives in the
    /// binary crate; here we only assert the clap dispatch + request plumbing).
    fn stub_job(_req: &JobRequest) -> i32 {
        12
    }

    /// A no-op world-apply: a `--commit` in a unit test "succeeds" without touching the World
    /// (the real interpreter-backed applier lives in the binary crate).
    fn noop_apply(_plan: &qfs_core::Plan) -> Result<(), qfs_exec::ExecError> {
        Ok(())
    }

    /// A stub run-context: an empty engine + empty read registry (read tests use the qfs-exec
    /// black-box API; the binary supplies the real local-driver context).
    fn stub_run_ctx() -> (Engine, qfs_exec::ReadRegistry, qfs_core::SafetyMode) {
        (
            Engine::new(),
            qfs_exec::ReadRegistry::new(),
            qfs_core::SafetyMode::default(),
        )
    }

    /// Run with the no-op shell + serve launchers + empty describe + stub skill + stub connection
    /// providers (every non-shell/serve/describe/skill/connection test path ignores them).
    fn run_t<I, T>(args: I) -> i32
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        run(
            args,
            &noop_shell,
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
            &stub_connection,
            &stub_identity,
            &stub_init,
            &stub_vault,
            &stub_invite,
            &stub_job,
            &noop_apply,
            &stub_run_ctx,
        )
    }

    #[test]
    fn vault_verbs_dispatch_through_the_injected_launcher() {
        // ADR 0008 §5: `qfs vault slots/enroll/revoke` route to the injected VaultLauncher (the
        // binary owns the slot I/O + keyring guardian). qfs-cmd only parses + forwards selectors.
        let seen: std::cell::RefCell<Option<VaultAction>> = std::cell::RefCell::new(None);
        let launcher = |action: &VaultAction| {
            *seen.borrow_mut() = Some(action.clone());
            13
        };
        let code = run(
            ["qfs", "vault", "enroll", "keychain"],
            &noop_shell,
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
            &stub_connection,
            &stub_identity,
            &stub_init,
            &launcher,
            &stub_invite,
            &stub_job,
            &noop_apply,
            &stub_run_ctx,
        );
        assert_eq!(code, 13, "vault dispatches to the launcher");
        assert_eq!(
            seen.borrow().as_ref(),
            Some(&VaultAction::Enroll {
                guardian: "keychain".into()
            })
        );
        assert_eq!(run_t(["qfs", "vault", "slots"]), 13);
        assert_eq!(run_t(["qfs", "vault", "revoke", "2"]), 13);
    }

    #[test]
    fn job_verbs_dispatch_through_the_injected_launcher() {
        // t65: `qfs job run` / `qfs job cron` route to the injected JobLauncher (the binary owns
        // the boot→build→gate→apply path). qfs-cmd only parses the verb + forwards the request.
        let seen: std::cell::RefCell<Option<JobRequest>> = std::cell::RefCell::new(None);
        let launcher = |req: &JobRequest| {
            *seen.borrow_mut() = Some(req.clone());
            7
        };
        let code = run(
            [
                "qfs",
                "job",
                "run",
                "/etc/qfs/app.qfs",
                "nightly",
                "--commit",
            ],
            &noop_shell,
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
            &stub_connection,
            &stub_identity,
            &stub_init,
            &stub_vault,
            &stub_invite,
            &launcher,
            &noop_apply,
            &stub_run_ctx,
        );
        assert_eq!(
            code, 7,
            "job dispatches to the launcher and returns its code"
        );
        let req = seen.borrow().clone().expect("launcher saw a request");
        assert_eq!(req.action, JobAction::Run);
        assert_eq!(req.name, "nightly");
        assert!(req.commit, "--commit plumbs through");
        assert!(!req.commit_irreversible);
        assert!(req.config.ends_with("app.qfs"));

        // `qfs job cron` plumbs the Cron action (no commit flags).
        let seen2: std::cell::RefCell<Option<JobRequest>> = std::cell::RefCell::new(None);
        let launcher2 = |req: &JobRequest| {
            *seen2.borrow_mut() = Some(req.clone());
            0
        };
        let _ = run(
            ["qfs", "job", "cron", "/etc/qfs/app.qfs", "nightly"],
            &noop_shell,
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
            &stub_connection,
            &stub_identity,
            &stub_init,
            &stub_vault,
            &stub_invite,
            &launcher2,
            &noop_apply,
            &stub_run_ctx,
        );
        assert_eq!(
            seen2.borrow().as_ref().expect("cron request").action,
            JobAction::Cron
        );
    }

    #[test]
    fn run_dispatch_resolves_single_statement_source() {
        // t29: `qfs run` now dispatches into the execution layer. Resolving exactly one
        // statement source is a usage gate; zero sources is exit 2 (usage).
        let code = dispatch_run(
            RunOpts {
                stmt: None,
                expr: None,
                format: Some("json".into()),
                json: true,
                commit: false,
                commit_irreversible: false,
                quiet: false,
            },
            &noop_apply,
            &stub_run_ctx,
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
            &stub_connection,
            &stub_identity,
            &stub_init,
            &stub_vault,
            &stub_invite,
            &stub_job,
            &noop_apply,
            &stub_run_ctx,
        );
        assert!(
            launched.get(),
            "no subcommand must invoke the shell launcher"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn run_bad_syntax_is_parse_error_exit_two() {
        // `qfs run -e '<garbage>'` reaches a structured parse error (exit 2), not a panic. Post-t73
        // a lone bare word is a valid source name, so use multi-token garbage that cannot parse.
        let code = run_t(["qfs", "run", "-e", "this is not pipe sql"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn run_relative_path_is_usage_error_exit_two() {
        // A relative-path address in one-shot mode is rejected with a usage error (exit 2).
        let code = run_t(["qfs", "run", "-e", "mail/inbox |> LIMIT 1"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn run_unknown_source_is_capability_exit_three() {
        // With no read driver registered, an absolute `/x` resolves to a structured
        // capability error (exit 3) — never a panic.
        let code = run_t(["qfs", "run", "-e", "/mail/inbox |> LIMIT 1", "--json"]);
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
            &stub_connection,
            &stub_identity,
            &stub_init,
            &stub_vault,
            &stub_invite,
            &stub_job,
            &noop_apply,
            &stub_run_ctx,
        );
        assert!(
            launched.get(),
            "serve must invoke the serve launcher with the config path"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn connection_verbs_map_to_the_public_action() {
        // The clap verb maps 1:1 to the injected-launcher action (selectors only, no secret).
        assert_eq!(
            connection_action(&ConnectionVerb::Add {
                driver: "mail".into(),
                connection: "work".into()
            }),
            ConnectionAction::Add {
                driver: "mail".into(),
                connection: "work".into()
            }
        );
        assert_eq!(
            connection_action(&ConnectionVerb::List { driver: None }),
            ConnectionAction::List { driver: None }
        );
        assert_eq!(
            connection_action(&ConnectionVerb::Use {
                driver: "s3".into(),
                connection: "prod".into()
            }),
            ConnectionAction::Use {
                driver: "s3".into(),
                connection: "prod".into()
            }
        );
        assert_eq!(
            connection_action(&ConnectionVerb::Remove {
                driver: "mail".into(),
                connection: "work".into()
            }),
            ConnectionAction::Remove {
                driver: "mail".into(),
                connection: "work".into()
            }
        );
    }

    #[test]
    fn connection_subcommand_parses_and_dispatches_to_the_launcher() {
        // `qfs connection …` parses cleanly and routes into the injected connection launcher (the stub
        // returns the sentinel 7). The real encrypted-store I/O lives in the binary crate.
        assert_eq!(run_t(["qfs", "connection", "list"]), 7);
        assert_eq!(run_t(["qfs", "connection", "add", "mail", "work"]), 7);
    }

    #[test]
    fn identity_verbs_map_to_the_public_action() {
        // The clap verb maps 1:1 to the injected-launcher action (handles only, no password).
        assert_eq!(
            identity_action(&IdentityVerb::Whoami { email: None }),
            IdentityAction::Whoami { email: None }
        );
        assert_eq!(
            identity_action(&IdentityVerb::Whoami {
                email: Some("a@b.com".into())
            }),
            IdentityAction::Whoami {
                email: Some("a@b.com".into())
            }
        );
    }

    #[test]
    fn identity_subcommand_parses_and_dispatches_to_the_launcher() {
        // `qfs identity …` parses cleanly and routes into the injected identity launcher (the stub
        // returns the sentinel 9). The real System-DB store I/O lives in the binary crate.
        // `identity signup` is RETIRED (ADR 0008 — `qfs init` replaced it): a hard parse error.
        assert_eq!(run_t(["qfs", "identity", "whoami"]), 9);
        assert_eq!(run_t(["qfs", "identity", "whoami", "a@b.com"]), 9);
        assert_eq!(
            run_t(["qfs", "identity", "signup", "a@b.com"]),
            2,
            "the retired signup verb no longer parses"
        );
    }

    #[test]
    fn init_dispatches_to_the_injected_launcher() {
        // ADR 0008 §2: `qfs init [email]` routes to the injected InitLauncher (the binary owns the
        // identity + vault I/O). The email is optional (a terminal prompts; automation passes it).
        let seen: std::cell::RefCell<Option<InitAction>> = std::cell::RefCell::new(None);
        let launcher = |action: &InitAction| {
            *seen.borrow_mut() = Some(action.clone());
            14
        };
        let code = run(
            ["qfs", "init", "you@example.com"],
            &noop_shell,
            &|_cfg| 0,
            &empty_describe,
            &stub_skill,
            &stub_connection,
            &stub_identity,
            &launcher,
            &stub_vault,
            &stub_invite,
            &stub_job,
            &noop_apply,
            &stub_run_ctx,
        );
        assert_eq!(code, 14);
        assert_eq!(
            seen.borrow().as_ref(),
            Some(&InitAction {
                email: Some("you@example.com".into())
            })
        );
        assert_eq!(run_t(["qfs", "init"]), 14, "the email is optional");
    }

    #[test]
    fn invite_verbs_map_to_the_public_action() {
        // The clap verb maps 1:1 to the injected-launcher action (selectors/handles only, no secret).
        assert_eq!(
            invite_action(&InviteVerb::Create {
                email: Some("a@b.com".into()),
                scope: None,
                project: None,
                role: None,
                ttl_secs: Some(3600)
            }),
            InviteAction::Create {
                email: Some("a@b.com".into()),
                scope: None,
                project: None,
                role: None,
                ttl_secs: Some(3600)
            }
        );
        assert_eq!(
            invite_action(&InviteVerb::Redeem {
                token: "tok".into(),
                email: "a@b.com".into()
            }),
            InviteAction::Redeem {
                token: "tok".into(),
                email: "a@b.com".into()
            }
        );
        assert_eq!(
            invite_action(&InviteVerb::Revoke { id: 7 }),
            InviteAction::Revoke { id: 7 }
        );
    }

    #[test]
    fn invite_subcommand_parses_and_dispatches_to_the_launcher() {
        // `qfs invite …` parses cleanly and routes into the injected invite launcher (the stub
        // returns the sentinel 11). The real System-DB store I/O lives in the binary crate.
        assert_eq!(run_t(["qfs", "invite", "create", "--email", "a@b.com"]), 11);
        assert_eq!(run_t(["qfs", "invite", "redeem", "tok", "a@b.com"]), 11);
        assert_eq!(run_t(["qfs", "invite", "revoke", "5"]), 11);
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
                &provider,
                &stub_connection,
                &stub_identity,
                &stub_init,
                &stub_vault,
                &stub_invite,
                &stub_job,
                &noop_apply,
                &stub_run_ctx
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
                &provider,
                &stub_connection,
                &stub_identity,
                &stub_init,
                &stub_vault,
                &stub_invite,
                &stub_job,
                &noop_apply,
                &stub_run_ctx
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
