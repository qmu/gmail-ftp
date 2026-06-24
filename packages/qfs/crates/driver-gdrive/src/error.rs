//! The structured Drive driver error (RFD-0001 §5: machine-readable for an AI, never prose,
//! and **never** secret-bearing). Every arm carries a stable [`DriveError::code`] and a
//! secret-free message: a path, a verb label, an HTTP status, an API op, or a fixed reason —
//! **never** a token, a header value, or file bytes. The auth base's
//! [`AuthError`](qfs_google_auth::AuthError) is mapped in at the boundary so no auth-internal
//! type leaks past the driver, preserving the t19 token discipline by construction.

use qfs_google_auth::AuthError;
use qfs_runtime::EffectError;

/// Why a Drive driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DriveError {
    /// A path did not resolve to a Drive node the driver services (outside `/drive`, an empty
    /// `id:`, or a Shared Drive path naming no drive). Structured: carries the offending path.
    #[error("path {path:?} is not a valid /drive address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A path segment did not resolve to a child of its parent folder (a missing name, or an
    /// ambiguous multi-parent placement). Structured: carries the path + the failing segment.
    #[error("could not resolve {segment:?} under {path:?}: {reason}")]
    NotFound {
        /// The VFS path being resolved.
        path: String,
        /// The path segment that did not resolve.
        segment: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A verb was attempted that the node does not support (e.g. `INSERT` of arbitrary columns
    /// against a blob node). Structured: names the path and the denied verb label.
    #[error("capability denied: cannot {verb} at {path:?}")]
    CapabilityDenied {
        /// The path the verb was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `INSERT`, `UPDATE`).
        verb: &'static str,
    },

    /// A `CALL` named a procedure the driver does not declare.
    #[error("unknown procedure: {0:?}")]
    UnknownProcedure(String),

    /// A write effect carried no usable payload (e.g. an UPSERT with no bytes, a rename with no
    /// new name). A construction/contract bug surfaced as terminal.
    #[error("malformed {verb} effect at {path:?}: {reason}")]
    MalformedEffect {
        /// The verb label.
        verb: &'static str,
        /// The path the effect targeted.
        path: String,
        /// A secret-free reason.
        reason: String,
    },

    /// The Drive API returned a non-2xx status. Structured: carries the status and the API op
    /// label (never the URL with a query, never a token). The `op` lets an AI branch on which
    /// call failed (e.g. `files.list`).
    #[error("drive api {op} returned status {status}")]
    Api {
        /// The Drive API operation label (e.g. `files.list`, `files.export`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A response body could not be decoded into the owned DTO (malformed/unexpected JSON).
    /// Carries the op label + a secret-free reason; **never** the body bytes.
    #[error("drive api {op} response could not be decoded: {reason}")]
    Decode {
        /// The Drive API operation label.
        op: &'static str,
        /// A secret-free reason (a JSON-shape note, never the payload).
        reason: String,
    },

    /// A Google-native doc (Docs/Sheets/Slides) has no raw bytes and no export target maps for
    /// the requested format. Structured: the source MIME type (a low-sensitivity label).
    #[error("no export target for google-native mime {mime:?}")]
    NoExportTarget {
        /// The source Google-native MIME type (e.g. `application/vnd.google-apps.document`).
        mime: String,
    },

    /// A codec failed to decode a downloaded body into rows (the read-to-rows path). Carries the
    /// secret-free codec error message (never the body bytes).
    #[error("could not decode the downloaded body: {reason}")]
    CodecDecode {
        /// A secret-free reason (the codec's class note).
        reason: String,
    },

    /// An auth/transport failure from the t19 auth base, reduced to its secret-free `code`.
    #[error("drive auth/transport failure: {code}")]
    Auth {
        /// The secret-free [`AuthError`] code (e.g. `auth_network`, `auth_token_refresh`).
        code: &'static str,
        /// Whether a re-authorize is required (a revoked/expired refresh grant).
        reauthorize: bool,
    },
}

impl DriveError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::NotFound { .. } => "not_found",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::UnknownProcedure(_) => "unknown_procedure",
            Self::MalformedEffect { .. } => "malformed_effect",
            Self::Api { .. } => "api_status",
            Self::Decode { .. } => "decode",
            Self::NoExportTarget { .. } => "no_export_target",
            Self::CodecDecode { .. } => "codec_decode",
            Self::Auth { .. } => "auth",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry on a reversible leg).
    /// A 5xx / 429 API status and a network auth failure are retryable; everything else is
    /// terminal. Trash legs are flagged `irreversible` upstream, so the runtime never retries
    /// them regardless.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            Self::Auth { code, .. } => *code == "auth_network",
            _ => false,
        }
    }
}

impl From<AuthError> for DriveError {
    /// Map an auth-base failure into the secret-free driver error, preserving only its stable
    /// `code` and the re-authorize signal. No token, endpoint URL, or header value crosses.
    fn from(err: AuthError) -> Self {
        DriveError::Auth {
            code: err.code(),
            reauthorize: err.is_reauthorize_required(),
        }
    }
}

impl From<DriveError> for EffectError {
    /// Reduce a Drive failure into the runtime's structured per-effect error so the
    /// interpreter's retry/ledger logic can branch on its class (RFD §5/§6). A capability
    /// denial maps to [`EffectError::CapabilityDenied`]; retryable classes map to
    /// [`EffectError::retryable`]; everything else is terminal. Every message is secret-free.
    fn from(err: DriveError) -> Self {
        match err {
            DriveError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("drive"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
