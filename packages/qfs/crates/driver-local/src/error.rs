//! The structured local-filesystem driver error (RFD-0001 §5: errors must be
//! machine-readable for an AI, never prose). Every arm carries a stable [`LocalError::code`]
//! and a secret-free message — full file contents are **never** rendered, only the path and
//! byte counts.

use qfs_runtime::EffectError;

/// Why a local-FS operation failed. Owned, vendor-free data; `std::io::Error` is mapped into
/// the [`LocalError::Io`] arm at the boundary so no `std::io` type leaks past the driver.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LocalError {
    /// A resolved path escaped the sandbox `root` (`..`, an absolute jump, or a symlink that
    /// canonicalises outside the mount). The blast-radius control — **no I/O is performed**.
    #[error("path {0:?} resolves outside the sandbox root")]
    OutsideSandbox(String),

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

    /// An underlying I/O failure, reduced to a secret-free message (path + reason, never
    /// file contents). `kind` preserves the `std::io::ErrorKind` label for AI branching.
    #[error("io error at {path:?}: {kind}")]
    Io {
        /// The path the I/O acted on.
        path: String,
        /// The stable `std::io::ErrorKind` debug label (e.g. `PermissionDenied`).
        kind: String,
    },
}

impl LocalError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::OutsideSandbox(_) => "outside_sandbox",
            Self::NotFound(_) => "not_found",
            Self::AlreadyExists(_) => "already_exists",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::VerifyFailed { .. } => "verify_failed",
            Self::Io { .. } => "io",
        }
    }

    /// Build an [`LocalError::Io`] from a `std::io::Error`, mapping `NotFound`/`AlreadyExists`
    /// to their dedicated arms and reducing the rest to a secret-free `(path, kind)` pair.
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
    /// Sandbox/capability/not-found/already-exists/verify are all **terminal** — only a raw
    /// `Io` failure is conservatively treated as retryable (a transient FS hiccup).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

impl From<LocalError> for EffectError {
    /// Map a local-FS failure into the runtime's structured per-effect error so the
    /// interpreter's retry/ledger logic — and the audit ledger — can branch on its class
    /// (RFD §5/§6). The discriminant is **preserved**, not collapsed: a sandbox-escape
    /// (`OutsideSandbox`) maps to the dedicated [`EffectError::SandboxEscape`] (`code =
    /// sandbox_escape`) and a capability denial (`CapabilityDenied`) maps to
    /// [`EffectError::CapabilityDenied`] (`code = capability_denied`), so an operator triaging a
    /// failed COMMIT can tell "I tried to escape the sandbox" from "I lacked permission" from
    /// the ledger alone. The remaining terminal classes carry their secret-free reason; `Io`
    /// is retryable on a reversible leg.
    fn from(err: LocalError) -> Self {
        match err {
            LocalError::OutsideSandbox(path) => EffectError::sandbox_escape(path),
            LocalError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("local"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
