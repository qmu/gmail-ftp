//! The structured Cloudflare driver error (RFD-0001 §5: errors are machine-readable for an AI,
//! never prose, and **never** secret-bearing). Every arm carries a stable [`CfError::code`] and
//! a secret-free message: a path, a verb label, a service + op label, an HTTP status, or a
//! fixed reason — **never** the API token, the `Authorization` header value, a key/message
//! payload, or any credential. The shared [`SqlError`](qfs_sql_core::SqlError) (reused for the
//! D1 SQL compile/emit leg) is mapped in at the boundary so no SQL-internal type leaks past the
//! driver.

use qfs_runtime::EffectError;
use qfs_sql_core::SqlError;

/// Why a Cloudflare driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CfError {
    /// A path did not resolve to a Cloudflare node the driver services (e.g. a path outside
    /// `/cf`, or an unknown service segment). Structured: carries the offending path.
    #[error("path {path:?} is not a valid /cf address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A verb was attempted that the node does not support (e.g. `UPDATE /cf/queue/q`, or a
    /// write over a KV namespace). Structured: names the path and the denied verb label.
    #[error("capability denied: cannot {verb} at {path:?}")]
    CapabilityDenied {
        /// The path the verb was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `UPDATE`, `JOIN`).
        verb: &'static str,
    },

    /// A write effect carried no usable payload (e.g. a KV upsert with no key, a queue insert
    /// with no body, or a D1 row that does not match the table catalog). A construction/contract
    /// bug surfaced as terminal.
    #[error("malformed {verb} effect at {path:?}: {reason}")]
    MalformedEffect {
        /// The verb label.
        verb: &'static str,
        /// The path the effect targeted.
        path: String,
        /// A secret-free reason.
        reason: String,
    },

    /// The Cloudflare API returned a non-2xx status. Structured: carries the status and the
    /// service + op label (never the URL with a token, never the bearer). The `op` lets an AI
    /// branch on which call failed (e.g. `d1.batch`, `kv.put`, `queue.send`).
    #[error("cloudflare {op} returned status {status}")]
    Api {
        /// The Cloudflare API operation label (e.g. `d1.query`, `kv.put`, `queue.send`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// A response body could not be decoded into the owned DTO (malformed/unexpected JSON, or
    /// a Cloudflare `success: false` envelope). Carries the op label + a secret-free reason;
    /// **never** the body bytes.
    #[error("cloudflare {op} response could not be decoded: {reason}")]
    Decode {
        /// The Cloudflare API operation label.
        op: &'static str,
        /// A secret-free reason (a JSON-shape note, never the payload).
        reason: String,
    },

    /// The Cloudflare API token could not be resolved (no credential for the account, or the
    /// store is locked). Carries the secret-free store error code, never the value.
    #[error("cloudflare auth resolution failed: {code}")]
    Auth {
        /// The secret-free store error code (e.g. `secret_not_found`, `secret_locked`).
        code: &'static str,
    },

    /// The transport failed before a status was received (DNS, connect, TLS, read timeout).
    /// Retry-safe only on a retry-safe leg (the runtime gates that by `irreversible`).
    #[error("cloudflare transport failure: {reason}")]
    Transport {
        /// A secret-free reason (the transport class, never a header value).
        reason: String,
    },

    /// The D1 SQL compile/emit leg (reused from t17 `qfs-driver-sql`) failed — e.g. an unknown
    /// column or table in the catalog. Reduced to a secret-free reason + the stable t17 code.
    #[error("cloudflare d1 sql error ({code}): {reason}")]
    D1Sql {
        /// The stable t17 [`SqlError`] code (e.g. `unknown_column`, `unknown_table`).
        code: &'static str,
        /// A secret-free reason (never a bound param VALUE).
        reason: String,
    },
}

impl CfError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::MalformedEffect { .. } => "malformed_effect",
            Self::Api { .. } => "api_status",
            Self::Decode { .. } => "decode",
            Self::Auth { .. } => "auth",
            Self::Transport { .. } => "transport",
            Self::D1Sql { .. } => "d1_sql",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry on a reversible leg).
    /// A 5xx / 429 API status and a transport failure are retryable; everything else is
    /// terminal. Irreversible legs (a queue send, a D1 destructive write) are flagged
    /// `irreversible` upstream, so the runtime never retries them regardless.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 || *status == 429,
            Self::Transport { .. } => true,
            _ => false,
        }
    }
}

impl From<SqlError> for CfError {
    /// Map the reused t17 SQL compile/emit failure into the secret-free CF error, preserving
    /// only its stable `code` and message. The t17 [`SqlError`] is already secret-free (it never
    /// renders a bound param value or a credential), so this carries no secret across.
    fn from(err: SqlError) -> Self {
        CfError::D1Sql {
            code: err.code(),
            reason: err.to_string(),
        }
    }
}

impl From<CfError> for EffectError {
    /// Reduce a Cloudflare failure into the runtime's structured per-effect error so the
    /// interpreter's retry/ledger logic can branch on its class (RFD §5/§6). A capability denial
    /// maps to [`EffectError::CapabilityDenied`]; retryable classes map to
    /// [`EffectError::retryable`]; everything else is terminal. Every message is secret-free.
    fn from(err: CfError) -> Self {
        match err {
            CfError::CapabilityDenied { path, verb } => EffectError::CapabilityDenied {
                driver: qfs_types::DriverId::new("cf"),
                verb: format!("{verb} at {path:?}"),
            },
            other if other.is_retryable() => EffectError::retryable(other.to_string()),
            other => EffectError::terminal(other.to_string()),
        }
    }
}
