//! The structured object-storage driver error (RFD-0001 §5: errors are machine-readable for an
//! AI, never prose, and **never** secret-bearing). Every arm carries a stable [`ObjError::code`]
//! and a secret-free message: a path, a verb label, a service + op label, an HTTP status, or a
//! fixed reason — **never** the access key, the secret key, the `Authorization` header value, the
//! object bytes, or any credential.

use qfs_runtime::EffectError;

/// Why an object-storage driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ObjError {
    /// A path did not resolve to an object-storage node the driver services (outside `/s3`/`/r2`,
    /// or missing a bucket/key). Structured: carries the offending path.
    #[error("path {path:?} is not a valid object-storage address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A verb was attempted that the node does not support (e.g. `UPDATE /s3/b/k`). Structured:
    /// names the path and the denied verb label.
    #[error("capability denied: cannot {verb} at {path:?}")]
    CapabilityDenied {
        /// The path the verb was attempted against.
        path: String,
        /// The denied verb's stable label.
        verb: &'static str,
    },

    /// A write effect carried no usable payload (e.g. an UPSERT with no key, or a REMOVE on a
    /// bucket root with no key). A construction/contract bug surfaced as terminal.
    #[error("malformed {verb} effect at {path:?}: {reason}")]
    MalformedEffect {
        /// The verb label.
        verb: &'static str,
        /// The path the effect targeted.
        path: String,
        /// A secret-free reason.
        reason: String,
    },

    /// The S3/R2 API returned a non-2xx status. Structured: carries the status and the op label
    /// (never the URL with a signature, never the `Authorization`). The `op` lets an AI branch on
    /// which call failed (e.g. `put_object`, `delete_object`, `list_objects_v2`).
    #[error("object-storage {op} returned status {status}")]
    Api {
        /// The S3/R2 API operation label.
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A response body could not be decoded into the owned DTO (malformed/unexpected XML/JSON).
    /// Carries the op label + a secret-free reason; **never** the body bytes.
    #[error("object-storage {op} response could not be decoded: {reason}")]
    Decode {
        /// The S3/R2 API operation label.
        op: &'static str,
        /// A secret-free reason (a shape note, never the payload).
        reason: String,
    },

    /// The transport failed before a status was received (DNS, connect, TLS, read timeout).
    /// Retry-safe only on a retry-safe leg (the runtime gates that by `irreversible`).
    #[error("object-storage transport failure: {reason}")]
    Transport {
        /// A secret-free reason (the transport class, never a header value).
        reason: String,
    },

    /// A multipart upload failed and was **aborted** to avoid orphan-part billing (RFD §6). The
    /// abort itself succeeded; this surfaces the original failure that triggered it.
    #[error("multipart upload aborted at part {part}: {reason}")]
    MultipartAborted {
        /// The 1-based part number the failure occurred at.
        part: u32,
        /// A secret-free reason.
        reason: String,
    },

    /// An optimistic-concurrency precondition failed: the object's ETag/version did not match the
    /// `If-Match`/`@versionId` the conditional op expected (typically a 412). Carries the version
    /// the world currently holds so the runtime can surface a typed conflict.
    #[error("optimistic-concurrency conflict: object holds version {version:?}")]
    Conflict {
        /// The version/ETag the object currently holds (secret-free, opaque token).
        version: String,
    },
}

impl ObjError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::MalformedEffect { .. } => "malformed_effect",
            Self::Api { .. } => "api_status",
            Self::Decode { .. } => "decode",
            Self::Transport { .. } => "transport",
            Self::MultipartAborted { .. } => "multipart_aborted",
            Self::Conflict { .. } => "conflict",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry on a reversible leg).
    /// A 5xx / 503 `SlowDown` / 429 API status and a transport failure are retryable; everything
    /// else is terminal. Irreversible legs are flagged `irreversible` upstream, so the runtime
    /// never retries them regardless.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            Self::Transport { .. } => true,
            _ => false,
        }
    }
}

impl From<ObjError> for EffectError {
    /// Reduce an object-storage failure into the runtime's structured per-effect error so the
    /// interpreter's retry/ledger logic can branch on its class (RFD §5/§6). A capability denial
    /// maps to [`EffectError::CapabilityDenied`]; a conflict to [`EffectError::Conflict`];
    /// retryable classes to [`EffectError::retryable`]; everything else is terminal. Every message
    /// is secret-free.
    fn from(err: ObjError) -> Self {
        match err {
            ObjError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("s3"),
                verb: format!("{verb} at {path:?}"),
            },
            ObjError::Conflict { version } => EffectError::conflict(version),
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
