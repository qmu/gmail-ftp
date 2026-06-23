//! [`GitHubError`] — the driver's structured, **secret-free** error taxonomy (RFD-0001 §5/§6).
//!
//! Every variant carries machine-facing detail only (op, status, path, a reason string) —
//! **never** the PAT, an `Authorization` header value, or any credential. The reason strings are
//! built from the request *shape* (op/path/status), so a token cannot reach them by construction
//! (the planted-canary test asserts this).

use cfs_runtime::EffectError;

/// Why a GitHub operation failed — the per-call error the driver surfaces. Secret-free:
/// constructed from the request shape and the response status, never from a header value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GitHubError {
    /// The path did not resolve to a GitHub node (not under the mount, unknown namespace).
    #[error("invalid GitHub path {path:?}: {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// The effect node carried a shape this driver cannot service (missing key, bad verb for the
    /// node) — a construction/contract error surfaced terminally (never a panic).
    #[error("malformed {verb} effect at {path:?}: {reason}")]
    MalformedEffect {
        /// The universal verb label.
        verb: &'static str,
        /// The effect path.
        path: String,
        /// A secret-free reason.
        reason: String,
    },

    /// The verb is not supported at this node (the parse-time capability gate's apply-leg twin).
    #[error("{verb} is not supported at {path:?}")]
    CapabilityDenied {
        /// The denied verb.
        verb: &'static str,
        /// The node path.
        path: String,
    },

    /// A `CALL github.<proc>` named a procedure this driver does not declare.
    #[error("unknown procedure: {0}")]
    UnknownProcedure(String),

    /// A non-2xx GitHub API status. Carries the op + status — **never** the auth header. A
    /// 401/403 surfaces here without the token, so the AI sees "this PAT is not authorized"
    /// rather than the credential that failed.
    #[error("GitHub API {op} returned status {status}")]
    Api {
        /// The op label (e.g. `issues.list`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A transport failure before a status was received (DNS, connect, TLS, timeout). Carries the
    /// op + a secret-free class reason (the transport's class, never a header value).
    #[error("GitHub API {op} transport error: {reason}")]
    Transport {
        /// The op label.
        op: &'static str,
        /// A secret-free transport class reason.
        reason: String,
    },

    /// The PAT could not be resolved from the credential store. Carries the secret-free store
    /// error code, never the value.
    #[error("auth resolution failed: {code}")]
    Auth {
        /// The secret-free store error code (e.g. `secret_not_found`, `secret_locked`).
        code: &'static str,
    },

    /// A response body could not be decoded into the owned DTO (the body's shape, never a
    /// credential — a GitHub JSON body carries no token).
    #[error("decode error for {op}: {reason}")]
    Decode {
        /// The op label.
        op: &'static str,
        /// A secret-free reason.
        reason: String,
    },
}

impl GitHubError {
    /// A short, stable machine code for structured surfaces and AI recovery.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            GitHubError::InvalidPath { .. } => "github_invalid_path",
            GitHubError::MalformedEffect { .. } => "github_malformed_effect",
            GitHubError::CapabilityDenied { .. } => "github_capability_denied",
            GitHubError::UnknownProcedure(_) => "github_unknown_procedure",
            GitHubError::Api { .. } => "github_api",
            GitHubError::Transport { .. } => "github_transport",
            GitHubError::Auth { .. } => "github_auth",
            GitHubError::Decode { .. } => "github_decode",
        }
    }

    /// Whether the status is transient (5xx / 429) — the retry-class gate.
    #[must_use]
    pub const fn is_transient_status(status: u16) -> bool {
        status >= 500 || status == 429
    }

    /// Lower this driver error into the runtime's [`EffectError`] recovery class. A transient
    /// API/transport failure becomes [`EffectError::retryable`] **only** when the method was
    /// retry-safe (an idempotent GET); a non-idempotent POST/PATCH transient failure is reported
    /// terminal so the interpreter never re-sends it (RFD §6 — at-least-once, no silent retry of
    /// non-idempotent writes).
    #[must_use]
    pub fn into_effect_error(self, method_retry_safe: bool) -> EffectError {
        match &self {
            GitHubError::Api { status, .. } if Self::is_transient_status(*status) => {
                if method_retry_safe {
                    EffectError::retryable(self.to_string())
                } else {
                    EffectError::terminal(self.to_string())
                }
            }
            GitHubError::Transport { .. } => {
                if method_retry_safe {
                    EffectError::retryable(self.to_string())
                } else {
                    EffectError::terminal(self.to_string())
                }
            }
            _ => EffectError::terminal(self.to_string()),
        }
    }
}

impl From<crate::client::TransportError> for GitHubError {
    /// Map a transport-seam error onto a secret-free [`GitHubError`]. Only the transport class
    /// crosses; the seam never carries a header value, so nothing secret can leak.
    fn from(e: crate::client::TransportError) -> Self {
        GitHubError::Transport {
            op: "http",
            reason: e.reason,
        }
    }
}
