//! The `qfs` first-class filesystem (`/fs`) composition root (t68): builds the operator-configured
//! [`FsRoots`] allowlist the binary injects into the live [`FsDriver`], and the driver itself.
//!
//! `/fs` addresses the REAL host filesystem under operator-named roots (an allowlist), widening
//! blast radius beyond the t28 `/local` sandbox — so the security floor is the headline (t68):
//! explicit root scoping, hard `..`/absolute/symlink escape rejection (validated at scan AND apply
//! time inside `qfs-driver-fs`), `POLICY` scopability per root, and `PREVIEW` before commit. The
//! `qfs-driver-fs` crate is a `qfs-runtime` consumer that must stay a LEAF — only the terminal
//! binary may depend on it — so the operator config lives HERE and the applier bridges into the
//! interpreter from the binary, exactly like the local / sql / git composition.
//!
//! ## Config (no credentials) — deny-all by default
//! Each root is one env var `QFS_FS_<NAME>=<absolute-base-dir>`; the `<NAME>` suffix (lower-cased)
//! is the `/fs/<name>/...` path segment selecting that base. **With no `QFS_FS_*` configured the
//! allowlist is empty — deny-all**: nothing resolves, so a `/fs` commit fails closed (no implicit
//! whole-disk access). A base that does not exist is kept (it simply fails closed on resolve).

use qfs_driver_fs::{FsDriver, FsRoots};

/// The env-var prefix naming an `/fs` root: `QFS_FS_<NAME>=<absolute-base-dir>`.
const FS_ENV_PREFIX: &str = "QFS_FS_";

/// Read the operator-configured [`FsRoots`] from the process environment. Each `QFS_FS_<NAME>`
/// maps the lower-cased `<NAME>` to its base directory. An empty result is the **deny-all default**
/// (no root configured ⇒ nothing resolves).
#[must_use]
pub fn configured_roots() -> FsRoots {
    let mut roots = FsRoots::new();
    for (key, value) in std::env::vars() {
        let Some(name) = key.strip_prefix(FS_ENV_PREFIX) else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        roots = roots.with_root(name.to_lowercase(), value);
    }
    roots
}

/// Whether any `/fs` root is configured (false ⇒ deny-all, so the binary leaves `/fs` unregistered
/// on the live apply registry rather than binding a driver that resolves nothing).
#[must_use]
pub fn has_roots() -> bool {
    std::env::vars().any(|(k, v)| k.starts_with(FS_ENV_PREFIX) && !v.is_empty())
}

/// Build the live, writable [`FsDriver`] over the operator-configured roots.
#[must_use]
pub fn fs_driver() -> FsDriver {
    FsDriver::new(configured_roots())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no `QFS_FS_*` set the allowlist is empty (deny-all) — the flagged default.
    #[test]
    fn no_env_is_deny_all() {
        // The test process sets no QFS_FS_* var, so the configured allowlist is empty.
        assert!(configured_roots().is_empty());
    }
}
