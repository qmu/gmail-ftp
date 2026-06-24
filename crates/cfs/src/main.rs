#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! `cfs` — the single binary (RFD-0001 §9: one Rust binary, both CLI and server).
//!
//! This entrypoint forwards argv to [`cfs_cmd::run`] and exits with the code it returns.
//! All argv parsing, dispatch, and rendering live in `cfs-cmd`; all domain logic lives below
//! `cfs-core`.
//!
//! The one piece the binary owns directly is the **interactive-shell launcher** (t28): the
//! shell's local-FS read facet depends on `cfs-driver-local` (a `cfs-runtime` consumer), and
//! only a leaf crate may carry that edge without tripping the runtime-confinement guard. The
//! binary is that leaf, so it builds the wired shell and injects it into `cfs-cmd` via the
//! [`cfs_cmd::ShellLauncher`]. The shell LOGIC itself lives in `cfs-exec`; this only wires it.

mod cron;
mod describe;
mod host;
mod serve;
mod serve_builtins;
mod shell;
mod watchtower;

fn main() {
    let code = cfs_cmd::run(
        std::env::args_os(),
        &shell::run_interactive_shell,
        &serve::run_serve,
        // t39: the describe-only driver registry (cred-free; only the pure introspective half is
        // ever called). Built here in the binary composition root; cfs-cmd stays off the driver
        // crates and consults it through the injected DescribeProvider.
        &describe::describe_registry,
        // t39 CO-t39-1: the embedded agent skill the binary ships. `cfs skill [--examples]` prints
        // `cfs_skill::render(..)` — this NORMAL `cfs → cfs-skill` edge is what makes SKILL.md ship in
        // the artifact and be discoverable from the running binary.
        &cfs_skill::render,
    );
    std::process::exit(code);
}
