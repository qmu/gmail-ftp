//! The structured `fs` driver error (RFD-0001 §5: errors must be machine-readable for an AI,
//! never prose). Every arm carries a stable [`FsError::code`] and a secret-free message — full
//! file contents are **never** rendered, only the path and byte counts.
//!
//! Templated on `qfs-driver-local`'s `LocalError`, with one added arm: [`FsError::UnknownRoot`]
//! — a path whose leading `/fs/<root>` segment names no operator-configured root. That arm is
//! the **deny-all default** made structural: with no root configured, every path's root lookup
//! misses and fails closed (no implicit whole-disk access).

use qfs_runtime::EffectError;

/// Why a `/fs` operation failed. Owned, vendor-free data; `std::io::Error` is mapped into the
/// [`FsError::Io`] arm at the boundary so no `std::io` type leaks past the driver.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FsError {
    /// The path's leading `/fs/<root>` segment names no operator-configured root (the
    /// **deny-all default**: an unconfigured/unknown root is denied, never guessed). No I/O is
    /// performed. The root NAME is secret-free; no host path is rendered.
    #[error("path {path:?} names no configured root {root:?}")]
    UnknownRoot {
        /// The offending VFS path.
        path: String,
        /// The unknown root segment name (secret-free label).
        root: String,
    },

    /// A resolved path escaped its configured root (`..`, an absolute jump, or a symlink that
    /// canonicalises outside the root). The blast-radius control — **no I/O is performed**.
    #[error("path {0:?} resolves outside its configured root")]
    OutsideRoot(String),

    /// The target path does not exist.
    #[error("path not found: {0:?}")]
    NotFound(String),

    /// A create/write expected the target to be absent but it already exists.
    #[error("path already exists: {0:?}")]
    AlreadyExists(String),

    /// A verb was attempted that the node does not support (e.g. a write on a `read_only`
    /// mount). Structured: names the path and the denied verb label (RFD §5).
    #[error("capability denied: cannot {verb} at {path:?}")]
    CapabilityDenied {
        /// The path the verb was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `UPSERT`, `RM`).
        verb: &'static str,
    },

    /// A `cp`/`mv` copy completed but the destination failed byte/size verification — the
    /// recovery guard (RFD §6): on `mv` the source is **never** unlinked when this fires.
    #[error("verification failed for {dst:?}: expected {expected} bytes, found {found}")]
    VerifyFailed {
        /// The destination whose verification failed.
        dst: String,
        /// The byte length the source reported.
        expected: u64,
        /// The byte length the destination actually held.
        found: u64,
    },

    /// An underlying I/O failure, reduced to a secret-free message (path + reason, never file
    /// contents). `kind` preserves the `std::io::ErrorKind` label for AI branching.
    #[error("io error at {path:?}: {kind}")]
    Io {
        /// The path the I/O acted on.
        path: String,
        /// The stable `std::io::ErrorKind` debug label (e.g. `PermissionDenied`).
        kind: String,
    },
}

impl FsError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownRoot { .. } => "unknown_root",
            Self::OutsideRoot(_) => "outside_root",
            Self::NotFound(_) => "not_found",
            Self::AlreadyExists(_) => "already_exists",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::VerifyFailed { .. } => "verify_failed",
            Self::Io { .. } => "io",
        }
    }

    /// Build an [`FsError::Io`] from a `std::io::Error`, mapping `NotFound`/`AlreadyExists` to
    /// their dedicated arms and reducing the rest to a secret-free `(path, kind)` pair.
    #[must_use]
    pub fn from_io(path: &str, err: &std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(path.to_string()),
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists(path.to_string()),
            kind => Self::Io {
                path: path.to_string(),
                kind: format!("{kind:?}"),
            },
        }
    }

    /// Whether this failure class is transient (worth a runtime retry on a reversible leg).
    /// Unknown-root/outside-root/capability/not-found/already-exists/verify are all **terminal**
    /// — only a raw `Io` failure is conservatively treated as retryable (a transient FS hiccup).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

impl From<FsError> for EffectError {
    /// Map an `fs` failure into the runtime's structured per-effect error so the interpreter's
    /// retry/ledger logic — and the audit ledger — can branch on its class (RFD §5/§6). The
    /// discriminant is **preserved**, not collapsed: a confinement breach (`UnknownRoot` /
    /// `OutsideRoot`) maps to the dedicated [`EffectError::SandboxEscape`] (`code =
    /// sandbox_escape`) and a capability denial maps to [`EffectError::CapabilityDenied`]
    /// (`code = capability_denied`), so an operator triaging a failed COMMIT can tell "I tried to
    /// reach outside a configured root" from "I lacked permission" from the ledger alone. The
    /// remaining terminal classes carry their secret-free reason; `Io` is retryable.
    fn from(err: FsError) -> Self {
        match err {
            FsError::UnknownRoot { path, .. } => EffectError::sandbox_escape(path),
            FsError::OutsideRoot(path) => EffectError::sandbox_escape(path),
            FsError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("fs"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
