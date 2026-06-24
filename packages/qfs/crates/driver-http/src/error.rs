//! [`HttpError`] — the driver's structured, **secret-free** error taxonomy (RFD-0001 §5/§6).
//!
//! Every variant carries machine-facing detail only (method, URL, status, a reason string) —
//! **never** an auth header value, token, or credential. The reason strings are built from
//! the request *shape* (method/URL/status), so a token cannot reach them by construction.
//! Each variant maps onto the runtime's recovery classes via [`HttpError::into_effect_error`]
//! so the interpreter's retry decision (retry transient, never retry a 4xx or a POST) falls
//! out of the error class.

use qfs_runtime::EffectError;

/// Why an HTTP exchange failed — the per-request error the driver surfaces. Secret-free:
/// constructed from the request shape and the response status, never from a header value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The transport failed before a status was received (DNS, connect, TLS, read timeout).
    /// Retry-safe **only** on a retry-safe method (`GET`/`PUT`/`DELETE`, never `POST`).
    #[error("transport error for {method} {url}: {reason}")]
    Transport {
        /// The HTTP method (uppercase token).
        method: String,
        /// The request URL (secret-free; query params are config passthrough, not tokens).
        url: String,
        /// A secret-free reason (the transport's class, never a header value).
        reason: String,
    },

    /// A 5xx server error or 429 rate-limit — transient. Retry-safe on a retry-safe method.
    #[error("server error {status} for {method} {url}")]
    Server {
        /// The HTTP method.
        method: String,
        /// The request URL.
        url: String,
        /// The HTTP status code (5xx or 429).
        status: u16,
    },

    /// A 4xx client error (bad request, unauthorized, not found, conflict) — terminal, never
    /// retried. A 401/403 surfaces here *without* the auth header, so the AI sees "this
    /// account is not authorized" rather than the token that failed.
    #[error("client error {status} for {method} {url}")]
    Client {
        /// The HTTP method.
        method: String,
        /// The request URL.
        url: String,
        /// The HTTP status code (4xx).
        status: u16,
    },

    /// The configured `secret_ref` could not be resolved (no credential for the account, or
    /// the store is locked). Carries the secret-free store error code, never the value.
    #[error("auth resolution failed: {code}")]
    Auth {
        /// The secret-free store error code (e.g. `secret_not_found`, `secret_locked`).
        code: &'static str,
    },

    /// The response body could not be decoded by the chosen codec (the codec's secret-free
    /// `detail`). The body is data, not a credential.
    #[error("decode error ({fmt}): {detail}")]
    Decode {
        /// The codec format (e.g. `json`).
        fmt: &'static str,
        /// The codec's machine-facing reason.
        detail: String,
    },

    /// A construction / contract error: the effect node carried a shape this driver cannot
    /// service (e.g. an unmapped resource, a `UPDATE`/`CALL` the REST driver does not do).
    #[error("invalid REST effect: {reason}")]
    Invalid {
        /// A secret-free reason.
        reason: String,
    },
}

impl HttpError {
    /// A short, stable machine code for structured surfaces and AI recovery.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            HttpError::Transport { .. } => "http_transport",
            HttpError::Server { .. } => "http_server",
            HttpError::Client { .. } => "http_client",
            HttpError::Auth { .. } => "http_auth",
            HttpError::Decode { .. } => "http_decode",
            HttpError::Invalid { .. } => "http_invalid",
        }
    }

    /// Map a response `status` (>= 400) plus the request shape onto the right error class.
    /// 5xx and 429 are transient ([`HttpError::Server`]); every other 4xx is terminal
    /// ([`HttpError::Client`]). Caller guarantees `status >= 400`.
    #[must_use]
    pub fn from_status(status: u16, method: &str, url: &str) -> Self {
        if status >= 500 || status == 429 {
            HttpError::Server {
                method: method.to_string(),
                url: url.to_string(),
                status,
            }
        } else {
            HttpError::Client {
                method: method.to_string(),
                url: url.to_string(),
                status,
            }
        }
    }

    /// Lower this driver error into the runtime's [`EffectError`] recovery class. The
    /// retry-safety of the *method* gates whether a transient class becomes `Retryable`: a
    /// `POST` transport/server failure is reported terminal so the interpreter never
    /// re-sends a non-idempotent create (RFD §6).
    #[must_use]
    pub fn into_effect_error(self, method_retry_safe: bool) -> EffectError {
        match self {
            HttpError::Transport { reason, .. } => {
                if method_retry_safe {
                    EffectError::retryable(reason)
                } else {
                    EffectError::terminal(reason)
                }
            }
            HttpError::Server {
                method,
                url,
                status,
            } => {
                let reason = format!("server error {status} for {method} {url}");
                if method_retry_safe {
                    EffectError::retryable(reason)
                } else {
                    EffectError::terminal(reason)
                }
            }
            other => EffectError::terminal(other.to_string()),
        }
    }
}
