//! `qfs::version` — the version + build-metadata surface (ticket t40, RFD §9).
//!
//! [`VERSION`] is the SemVer from `Cargo.toml` (`env!("CARGO_PKG_VERSION")`). The build
//! metadata ([`GIT_SHA`], [`TARGET`], [`WASM_CAPABLE`]) is baked in by `build.rs` via
//! `rustc-env`. [`long_version`] assembles the multi-line `qfs --version` long form — the
//! field-debug anchor an operator reads to know *exactly* which build is running.
//!
//! No secret is embedded here (RFD §10): a semver, a commit hash, a target triple, and a
//! derived boolean only.

/// The crate SemVer (`CARGO_PKG_VERSION`). The **stable surface is the grammar**, versioned
/// per the SemVer policy documented in `README.md`: a breaking grammar change is a major bump.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The short git sha the binary was built from (`unknown` for a non-git source build).
/// Baked in by `build.rs`.
pub const GIT_SHA: &str = env!("QFS_GIT_SHA");

/// The target triple this binary was compiled for (e.g. `x86_64-unknown-linux-musl`).
/// Baked in by `build.rs`.
pub const TARGET: &str = env!("QFS_TARGET");

/// Whether this build's target is `wasm32-*` capable (RFD §9 Cloudflare Workers target).
/// The string `"true"` / `"false"`, baked in by `build.rs`.
pub const WASM_CAPABLE: &str = env!("QFS_WASM_CAPABLE");

/// The long `qfs --version` form: semver + git sha + target triple (+ the wasm-capable flag),
/// one field per line. This is the clap `long_version`, surfaced on `qfs --version`.
///
/// The output is deterministic for a given build, so it is golden-testable.
#[must_use]
pub fn long_version() -> String {
    format!(
        "qfs {VERSION}\n\
         commit:  {GIT_SHA}\n\
         target:  {TARGET}\n\
         wasm32:  {WASM_CAPABLE}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The long form carries the four field-debug anchors in a stable, line-oriented shape.
    #[test]
    fn long_version_carries_semver_sha_and_target() {
        let v = long_version();
        assert!(
            v.starts_with(&format!("qfs {VERSION}")),
            "leads with semver"
        );
        assert!(v.contains("commit:"), "carries the git sha line");
        assert!(v.contains(&format!("target:  {TARGET}")), "carries target");
        assert!(v.contains("wasm32:"), "carries the wasm-capable flag");
        // No credential shape ever leaks into the version banner (RFD §10).
        assert!(!v.to_lowercase().contains("token"));
        assert!(!v.contains("Bearer"));
    }

    /// `VERSION` is a real semver triple (the SemVer policy's anchor).
    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "VERSION must be MAJOR.MINOR.PATCH: {VERSION}"
        );
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "semver component must be numeric: {p}"
            );
        }
    }
}
