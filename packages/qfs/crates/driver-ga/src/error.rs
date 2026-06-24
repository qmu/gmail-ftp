//! The structured Google Analytics driver error (RFD-0001 §5: machine-readable for an AI, never
//! prose, and **never** secret-bearing). Every arm carries a stable [`GaError::code`] and a
//! secret-free message: a path, a verb label, an HTTP status, an API op, a dimension/metric name,
//! or a fixed reason — **never** a token, a header value, or a credential. The auth base's
//! [`AuthError`](qfs_google_auth::AuthError) is mapped in at the boundary so no auth-internal type
//! leaks past the driver, preserving the t19 token discipline by construction.

use qfs_google_auth::AuthError;
use qfs_runtime::EffectError;

/// Why a Google Analytics driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GaError {
    /// A path did not resolve to a GA node the driver services (outside `/ga`, an empty property
    /// id, or an unexpected trailing segment). Structured: carries the offending path.
    #[error("path {path:?} is not a valid /ga address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A write verb (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`/…) was attempted against a `/ga` node.
    /// GA is a read-only query source; this is the read-only enforcement at the applier boundary
    /// (the parse-time capability gate rejects it first). Structured: names the path and verb.
    #[error("capability denied: /ga is read-only; cannot {verb} at {path:?}")]
    ReadOnly {
        /// The path the write was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `INSERT`, `UPDATE`).
        verb: &'static str,
    },

    /// A query carried no usable date range. GA4 **requires** a date range on every report;
    /// the compiler refuses to fabricate one silently. Structured so an AI can self-correct by
    /// adding a `WHERE date BETWEEN …` predicate.
    #[error("missing date range: a /ga report requires a `date` predicate (e.g. WHERE date BETWEEN '2024-01-01' AND '2024-01-31')")]
    MissingDateRange,

    /// A `SELECT` projected no dimensions and no metrics, so there is nothing for GA to report.
    /// Structured: an AI should project at least one metric.
    #[error("empty projection: a /ga report must SELECT at least one dimension or metric")]
    EmptyProjection,

    /// A projected/filtered name resolved to neither a dimension nor a metric in the property's
    /// catalog (or an incompatible dimension×metric combination). Structured: the offending name
    /// and a secret-free reason, so an AI can fix the query rather than hit a raw GA 400.
    #[error("unknown or incompatible field {name:?}: {reason}")]
    UnknownField {
        /// The offending dimension/metric name.
        name: String,
        /// A secret-free reason (e.g. "not in the property catalog").
        reason: &'static str,
    },

    /// The GA4 Data API returned a non-2xx status. Structured: carries the status and the API op
    /// label (never the URL with a query, never a token). The `op` lets an AI branch on which
    /// call failed (e.g. `runReport`).
    #[error("ga api {op} returned status {status}")]
    Api {
        /// The GA4 API operation label (e.g. `runReport`, `getMetadata`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A response body could not be decoded into the owned DTO (malformed/unexpected JSON).
    /// Carries the op label + a secret-free reason; **never** the body bytes.
    #[error("ga api {op} response could not be decoded: {reason}")]
    Decode {
        /// The GA4 API operation label.
        op: &'static str,
        /// A secret-free reason (a JSON-shape note, never the payload).
        reason: String,
    },

    /// An auth/transport failure from the t19 auth base, reduced to its secret-free `code`.
    #[error("ga auth/transport failure: {code}")]
    Auth {
        /// The secret-free [`AuthError`] code (e.g. `auth_network`, `auth_token_refresh`).
        code: &'static str,
        /// Whether a re-authorize is required (a revoked/expired refresh grant).
        reauthorize: bool,
    },
}

impl GaError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::ReadOnly { .. } => "read_only",
            Self::MissingDateRange => "missing_date_range",
            Self::EmptyProjection => "empty_projection",
            Self::UnknownField { .. } => "unknown_field",
            Self::Api { .. } => "api_status",
            Self::Decode { .. } => "decode",
            Self::Auth { .. } => "auth",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry). A 5xx / 429 API status
    /// (GA4 `RESOURCE_EXHAUSTED` quota exhaustion surfaces as 429) and a network auth failure are
    /// retryable; everything else is terminal.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            Self::Auth { code, .. } => *code == "auth_network",
            _ => false,
        }
    }
}

impl From<AuthError> for GaError {
    /// Map an auth-base failure into the secret-free driver error, preserving only its stable
    /// `code` and the re-authorize signal. No token, endpoint URL, or header value crosses.
    fn from(err: AuthError) -> Self {
        GaError::Auth {
            code: err.code(),
            reauthorize: err.is_reauthorize_required(),
        }
    }
}

impl From<GaError> for EffectError {
    /// Reduce a GA failure into the runtime's structured per-effect error so the interpreter's
    /// retry/ledger logic can branch on its class (RFD §5/§6). A read-only denial maps to
    /// [`EffectError::CapabilityDenied`]; retryable classes map to [`EffectError::retryable`];
    /// everything else is terminal. Every message is secret-free.
    fn from(err: GaError) -> Self {
        match err {
            GaError::ReadOnly { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("ga"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
