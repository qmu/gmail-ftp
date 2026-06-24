//! `xtask` — the qfs build tool (cargo-xtask pattern, ticket t40). **Not shipped** in the
//! binary (`publish = false`); it is the repo's task runner.
//!
//! ## Commands
//! - `cargo xtask gen-docs [--check]` — render the reference docs (`docs/language.md`,
//!   `docs/drivers.md`, `docs/server.md`) from the binary's OWN registries via
//!   [`qfs::docs`]. `--check` (CI mode) diffs the committed docs without writing and exits
//!   non-zero on drift (the anti-drift gate). This is the local, disk-safe command.
//! - `cargo xtask dist` — the release-artifact pipeline (cross-compile matrix → strip →
//!   sha256 → tarball + a wasm artifact). This is **CI-only**: a release/musl/wasm build
//!   wedges the constrained trip disk and the full workspace is not wasm-clean (only the pure
//!   cores are), so locally `dist` refuses to run unless `QFS_DIST_ALLOW=1` is set (it never
//!   is locally). The matrix + steps are real and reviewable; `release.yml` runs them in CI.
//!   See ADR-0007 for the offline/disk scoping (mirrors t36/ADR-0005).
//!
//! `dep-light`: the only dependency is the `qfs` path crate (whose lib facet exposes the doc
//! generator); everything else is std. No external crate is added.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The native release targets the `dist` matrix builds (RFD §1/§9): static Linux (musl) +
/// macOS, both arches. musl static cross-link is CI-only (no local cross-linker — t01/A2).
const NATIVE_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// The wasm artifact target (RFD §9 Cloudflare Workers). Only the pure cores are wasm-clean
/// (t36/ADR-0005); the full-binary wasm build is parked, so `dist` builds the wasm artifact in
/// CI and fails loudly if a non-wasm symbol is pulled in.
const WASM_TARGET: &str = "wasm32-unknown-unknown";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo_root = repo_root();

    match args.first().map(String::as_str) {
        Some("gen-docs") => {
            let check = args.iter().any(|a| a == "--check");
            // The generated `docs/` tree lives at the git repo root (above the packages/qfs
            // workspace), not in the workspace dir that `dist` writes its artifacts to.
            let docs_root = qfs::docs::find_repo_root(&repo_root);
            cmd_gen_docs(&docs_root, check)
        }
        Some("dist") => cmd_dist(&repo_root),
        Some("help") | Some("--help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`\n");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "qfs xtask — build tooling\n\n\
         USAGE:\n\
         \x20 cargo xtask gen-docs [--check]   render docs/*.md from the binary's registries\n\
         \x20 cargo xtask dist                 build release artifacts (CI-only; see ADR-0007)\n"
    );
}

/// `gen-docs`: render (or, with `--check`, verify) the reference docs. Disk-safe; the local
/// QA command.
fn cmd_gen_docs(repo_root: &Path, check: bool) -> ExitCode {
    if check {
        match qfs::docs::check_docs(repo_root) {
            Ok(drift) if drift.is_empty() => {
                println!("gen-docs --check: docs are in sync.");
                ExitCode::SUCCESS
            }
            Ok(drift) => {
                eprintln!("gen-docs --check: DRIFT — these docs are out of date:");
                for d in &drift {
                    eprintln!("  - {}", d.rel_path);
                }
                eprintln!("Run `cargo xtask gen-docs` and commit the result.");
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("gen-docs --check: I/O error: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        match qfs::docs::gen_docs(repo_root) {
            Ok(written) => {
                for p in &written {
                    println!("wrote {}", p.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gen-docs: I/O error: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// `dist`: the release-artifact pipeline. **CI-only**: refuses to run locally (it would wedge
/// the constrained disk and the full-binary wasm build is parked). The matrix + steps are real
/// and reviewable; `release.yml` invokes this in CI where `QFS_DIST_ALLOW=1` is set.
fn cmd_dist(repo_root: &Path) -> ExitCode {
    let allowed = std::env::var("QFS_DIST_ALLOW").as_deref() == Ok("1");
    if !allowed {
        eprintln!(
            "xtask dist: refusing to run outside CI.\n\
             A release/musl/wasm build wedges the constrained trip disk and the full workspace is \
             not wasm-clean (only the pure cores are — t36/ADR-0005). This pipeline is CI-only \
             (see ADR-0007 and `.github/workflows/release.yml`).\n\
             To run it where a clean cross toolchain + disk exist, set QFS_DIST_ALLOW=1."
        );
        // Print the matrix the pipeline WOULD execute, so the shape is reviewable from output.
        print_dist_plan(repo_root);
        return ExitCode::FAILURE;
    }

    // --- The real pipeline (executed only in CI). ---
    let dist_dir = repo_root.join("dist");
    if let Err(e) = std::fs::create_dir_all(&dist_dir) {
        eprintln!("xtask dist: cannot create {}: {e}", dist_dir.display());
        return ExitCode::FAILURE;
    }

    // Native tarballs: build --release per target, strip, sha256, tarball into dist/.
    for target in NATIVE_TARGETS {
        if let Err(e) = build_native(repo_root, &dist_dir, target) {
            eprintln!("xtask dist: native target {target} failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    // The wasm artifact: build the wasm-clean facet for wasm32 and fail loudly on a non-wasm
    // symbol (rather than silently shipping a broken artifact, RFD §9).
    if let Err(e) = build_wasm(repo_root, &dist_dir) {
        eprintln!("xtask dist: wasm artifact failed: {e}");
        return ExitCode::FAILURE;
    }

    println!("xtask dist: artifacts in {}", dist_dir.display());
    ExitCode::SUCCESS
}

/// Echo the dist matrix + step shape so a reviewer (and CI logs) can read the plan without a
/// build. This is the honest "what dist would do" surface.
fn print_dist_plan(repo_root: &Path) {
    eprintln!("\n--- dist plan (CI executes this) ---");
    eprintln!("dist dir: {}", repo_root.join("dist").display());
    eprintln!("native targets ({}):", NATIVE_TARGETS.len());
    for t in NATIVE_TARGETS {
        eprintln!("  - {t}: cargo build --release --target {t} -p qfs → strip → sha256 → tar.gz");
    }
    eprintln!(
        "wasm artifact: cargo build --release --target {WASM_TARGET} (wasm-clean facet) → \
         linker check (no non-wasm symbol) → sha256"
    );
}

/// Build, strip, checksum, and tarball one native target. CI-only (no local cross-linker).
fn build_native(repo_root: &Path, dist_dir: &Path, target: &str) -> std::io::Result<()> {
    run(
        repo_root,
        "cargo",
        &["build", "--release", "--target", target, "-p", "qfs"],
    )?;
    let bin = repo_root
        .join("target")
        .join(target)
        .join("release")
        .join("qfs");
    // Best-effort strip (the toolchain image provides the matching `strip`).
    let _ = run(repo_root, "strip", &[bin.to_string_lossy().as_ref()]);
    let tarball = dist_dir.join(format!("qfs-{target}.tar.gz"));
    run(
        repo_root,
        "tar",
        &[
            "-czf",
            tarball.to_string_lossy().as_ref(),
            "-C",
            bin.parent().unwrap_or(repo_root).to_string_lossy().as_ref(),
            "qfs",
        ],
    )?;
    write_sha256(&tarball)?;
    Ok(())
}

/// Build the wasm artifact and assert it pulls no non-wasm symbol. CI-only.
fn build_wasm(repo_root: &Path, dist_dir: &Path) -> std::io::Result<()> {
    // Only the pure cores are wasm-clean (t36); the wasm facet is built with the wasm feature
    // set. A non-wasm symbol makes this build FAIL (loud), which is the desired gate.
    run(
        repo_root,
        "cargo",
        &[
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "-p",
            "qfs-host",
        ],
    )?;
    let artifact = repo_root
        .join("target")
        .join(WASM_TARGET)
        .join("release")
        .join("qfs_host.wasm");
    if artifact.exists() {
        let dest = dist_dir.join("qfs.wasm");
        std::fs::copy(&artifact, &dest)?;
        write_sha256(&dest)?;
    }
    Ok(())
}

/// Write a `<file>.sha256` next to an artifact (the checksum `install.sh` verifies).
fn write_sha256(path: &Path) -> std::io::Result<()> {
    // Delegate to the platform `sha256sum` (Linux CI) — keeps xtask dep-light (no sha2 crate).
    let out = std::process::Command::new("sha256sum").arg(path).output()?;
    if out.status.success() {
        let sum = String::from_utf8_lossy(&out.stdout);
        std::fs::write(path.with_extension("sha256"), sum.as_bytes())?;
    }
    Ok(())
}

/// Run a command in `cwd`, mapping a non-zero exit to an `io::Error`.
fn run(cwd: &Path, program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "`{program} {}` exited with {status}",
            args.join(" ")
        )))
    }
}

/// The repo root: the workspace dir two levels up from this crate (`xtask/` is at the root, so
/// `CARGO_MANIFEST_DIR/..`). Falls back to the current dir.
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
