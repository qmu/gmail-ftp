//! The structured Gmail driver error (RFD-0001 §5: errors are machine-readable for an AI,
//! never prose, and **never** secret-bearing). Every arm carries a stable [`GmailError::code`]
//! and a secret-free message: a path, a verb label, an HTTP status, an account email (a
//! low-sensitivity id), or a fixed reason — **never** a token, a header value, or a message
//! body. The auth base's [`AuthError`](cfs_google_auth::AuthError) is mapped in at the
//! boundary so no auth-internal type leaks past the driver, and the token discipline of t19
//! (the bearer lives behind a [`cfs_secrets::Secret`] and the redacting `HttpRequest` `Debug`)
//! is preserved by construction.

use cfs_google_auth::AuthError;
use cfs_runtime::EffectError;

/// Why a Gmail driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GmailError {
    /// A path did not resolve to a Gmail node the driver services (e.g. a malformed `id:`
    /// address, or a path outside `/mail`). Structured: carries the offending path.
    #[error("path {path:?} is not a valid /mail address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A verb was attempted that the node does not support (e.g. `UPDATE /mail/drafts`, or an
    /// `INSERT` against a message). Structured: names the path and the denied verb label.
    #[error("capability denied: cannot {verb} at {path:?}")]
    CapabilityDenied {
        /// The path the verb was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `UPDATE`, `INSERT`).
        verb: &'static str,
    },

    /// A `CALL` named a procedure the driver does not declare. Only `mail.send` is callable.
    #[error("unknown procedure: {0:?} (the Gmail driver declares only `mail.send`)")]
    UnknownProcedure(String),

    /// A write effect carried no usable payload (e.g. a draft INSERT with no recipients/body,
    /// or a label UPDATE naming no labels). A construction/contract bug surfaced as terminal.
    #[error("malformed {verb} effect at {path:?}: {reason}")]
    MalformedEffect {
        /// The verb label.
        verb: &'static str,
        /// The path the effect targeted.
        path: String,
        /// A secret-free reason.
        reason: String,
    },

    /// The Gmail API returned a non-2xx status. Structured: carries the status and the API op
    /// label (never the URL with a query, never a token). The `op` lets an AI branch on which
    /// call failed (e.g. `messages.send`).
    #[error("gmail api {op} returned status {status}")]
    Api {
        /// The Gmail API operation label (e.g. `messages.list`, `drafts.send`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A response body could not be decoded into the owned DTO (malformed/unexpected JSON).
    /// Carries the op label + a secret-free reason; **never** the body bytes.
    #[error("gmail api {op} response could not be decoded: {reason}")]
    Decode {
        /// The Gmail API operation label.
        op: &'static str,
        /// A secret-free reason (a JSON-shape note, never the payload).
        reason: String,
    },

    /// The MIME builder could not construct a valid RFC 5322 message (e.g. no recipients).
    #[error("could not build the MIME message: {reason}")]
    Mime {
        /// A secret-free reason.
        reason: &'static str,
    },

    /// An auth/transport failure from the t19 auth base, reduced to its secret-free `code`.
    /// The underlying [`AuthError`] is already token-free; we carry only its stable code so an
    /// AI can branch (e.g. `auth_token_refresh` → re-authorize).
    #[error("gmail auth/transport failure: {code}")]
    Auth {
        /// The secret-free [`AuthError`] code (e.g. `auth_network`, `auth_token_refresh`).
        code: &'static str,
        /// Whether a re-authorize is required (a revoked/expired refresh grant).
        reauthorize: bool,
    },
}

impl GmailError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::UnknownProcedure(_) => "unknown_procedure",
            Self::MalformedEffect { .. } => "malformed_effect",
            Self::Api { .. } => "api_status",
            Self::Decode { .. } => "decode",
            Self::Mime { .. } => "mime",
            Self::Auth { .. } => "auth",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry on a reversible leg).
    /// A 5xx / 429 API status and a network auth failure are retryable; everything else is
    /// terminal. `mail.send`/trash legs are flagged `irreversible` upstream, so the runtime
    /// never retries them regardless.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            Self::Auth { code, .. } => *code == "auth_network",
            _ => false,
        }
    }
}

impl From<AuthError> for GmailError {
    /// Map an auth-base failure into the secret-free driver error, preserving only its stable
    /// `code` and the re-authorize signal. No token, endpoint URL, or header value crosses.
    fn from(err: AuthError) -> Self {
        GmailError::Auth {
            code: err.code(),
            reauthorize: err.is_reauthorize_required(),
        }
    }
}

impl From<GmailError> for EffectError {
    /// Reduce a Gmail failure into the runtime's structured per-effect error so the
    /// interpreter's retry/ledger logic can branch on its class (RFD §5/§6). A capability
    /// denial maps to [`EffectError::CapabilityDenied`]; retryable classes map to
    /// [`EffectError::retryable`]; everything else is terminal. Every message is secret-free.
    fn from(err: GmailError) -> Self {
        match err {
            GmailError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: cfs_types::DriverId::new("mail"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
