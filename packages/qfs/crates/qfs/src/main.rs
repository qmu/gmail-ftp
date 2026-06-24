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

use qfs::{describe, serve, shell, version};

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
    );
    std::process::exit(code);
}
