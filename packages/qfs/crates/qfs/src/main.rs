#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! `qfs` — the single binary (RFD-0001 §9: one Rust binary, both CLI and server).
//!
//! This entrypoint forwards argv to [`qfs_cmd::run`] and exits with the code it returns.
//! All argv parsing, dispatch, and rendering live in `qfs-cmd`; all domain logic lives below
//! `qfs-core`.
//!
//! The one piece the binary owns directly is the **interactive-shell launcher** (t28): the
//! shell's local-FS read facet depends on `qfs-driver-local` (a `qfs-runtime` consumer), and
//! only a leaf crate may carry that edge without tripping the runtime-confinement guard. The
//! binary is that leaf, so it builds the wired shell and injects it into `qfs-cmd` via the
//! [`qfs_cmd::ShellLauncher`]. The shell LOGIC itself lives in `qfs-exec`; this only wires it.

use qfs::{
    account, commit, connection, describe, identity, init, invite, job, serve, shell, store, vault,
    version,
};

fn main() {
    // t40: the binary owns the build metadata (semver + git sha + target triple baked in by
    // `build.rs`), so `qfs --version` / `-V` is intercepted HERE — before qfs-cmd's clap parse —
    // and printed in long form. qfs-cmd stays off the build-metadata surface; clap's own
    // `--version` is reserved for the qfs-cmd help machinery only. We match the exact, standalone
    // version flag as the sole argument so it never shadows a subcommand's own argument.
    let mut argv = std::env::args_os();
    let _bin = argv.next();
    let rest: Vec<std::ffi::OsString> = argv.collect();
    if rest.len() == 1 && (rest[0] == "--version" || rest[0] == "-V") {
        println!("{}", version::long_version());
        std::process::exit(0);
    }

    // t42: open the per-host System DB and apply embedded migrations on start (idempotent — a
    // second start is a no-op). This is start-time INFRASTRUCTURE, not a qfs effect-plan: it never
    // goes through preview/commit. t42 wires the seam WITHOUT routing any command through it (the
    // file vault still backs secrets until t43), so it is best-effort — a host with no config home
    // or a transient open error must not block the CLI. We log at debug and continue.
    match store::open_system_db() {
        Ok(Some(_sys)) => tracing::debug!("qfs: system DB migrations applied/verified on start"),
        Ok(None) => {}
        Err(e) => tracing::debug!("qfs: system DB unavailable on start (continuing): {e}"),
    }

    let code = qfs_cmd::run(
        std::env::args_os(),
        &shell::run_interactive_shell,
        &serve::run_serve,
        // t39: the describe-only driver registry (cred-free; only the pure introspective half is
        // ever called). Built here in the binary composition root; qfs-cmd stays off the driver
        // crates and consults it through the injected DescribeProvider.
        &describe::describe_registry,
        // t39 CO-t39-1: the embedded agent skill the binary ships. `qfs skill [--examples]` prints
        // `qfs_skill::render(..)` — this NORMAL `qfs → qfs-skill` edge is what makes SKILL.md ship in
        // the artifact and be discoverable from the running binary.
        &qfs_skill::render,
        // `qfs connection add/list/use/remove`: the real credential-store I/O, injected here (the
        // binary owns the envelope-encrypted SQLite store over the Project DB — t43; qfs-cmd stays
        // off the concrete backend). The secret is read from stdin, never argv; each value is
        // AEAD-sealed under a data-key wrapped by the `QFS_PASSPHRASE`-derived key.
        &connection::run_connection,
        // t45 `qfs identity whoami`: the System-DB-backed identity store I/O, injected here
        // (the binary owns the rusqlite store over the System DB — qfs-cmd stays off the concrete
        // backend). The password is read from stdin, never argv, hashed with argon2id (the plaintext
        // is zeroized after); the password hash is never printed. AUTHENTICATION ONLY — no session
        // yet (t46), no authorization (M2).
        &identity::run_identity,
        // ADR 0008 §2 `qfs init`: the first-run wizard — the System-DB operator identity (no
        // password: OS-delegated auth, unusable placeholder hash) + the vault creation through the
        // guardian flow, injected here (qfs-cmd stays off the concrete backends).
        &init::run_init,
        // ADR 0008 §3 `qfs app` / `qfs account`: the per-layer verbs over the vault + consent
        // ledger + the live Google consent seam, injected here (qfs-cmd stays off the backends).
        &account::run_account,
        // ADR 0008 §5 `qfs vault slots/enroll/revoke`: the KeyGuardian slot I/O + the OS-keyring
        // guardian, injected here (the binary owns the envelope store and the keyring dep; qfs-cmd
        // stays off both). Key material never crosses this seam — the action carries selectors only.
        &vault::run_vault,
        // t55 `qfs invite create/redeem/revoke`: the System-DB-backed invite store I/O + the
        // binary-owned CSPRNG that mints the one-time token, injected here (qfs-cmd stays off the
        // concrete backend). The token is generated, returned ONCE, and stored only as a hash; redeem
        // creates a real identity + membership (identity ≠ authorization, §4.1). Email delivery + the
        // HTTP accept-route session are documented seams (see crates/qfs/src/invite.rs).
        &invite::run_invite,
        // t65 `qfs job run/cron`: the EXTERNAL-scheduler entrypoint. The internal scheduler daemon
        // is retired (decision M revised) — a JOB is a saved named plan + cadence that OS cron /
        // Cloudflare Cron Triggers invoke. The binary owns the boot→rehydrate→build→policy-gate→
        // IrreversibleGuard→real-apply path (qfs-host/qfs-exec/qfs-runtime); qfs-cmd stays off them.
        &job::run_job_request,
        // The REAL `qfs run --commit` apply path: drives the qfs-runtime interpreter over the live
        // driver registry (local-fs today). qfs-cmd/qfs-exec stay off qfs-runtime; this is the leaf.
        &commit::apply_plan,
        // The run context for `qfs run`: the Engine (local-FS driver in its mounts so a `FROM …`
        // resolves + plans) + the ReadRegistry (the scan facet), so `/local/<p>` scans the
        // host `/<p>`. The binary owns the runtime-coupled adapter.
        &shell::run_engine_and_reads,
    );
    std::process::exit(code);
}
