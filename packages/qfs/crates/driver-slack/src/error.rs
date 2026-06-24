//! [`SlackError`] — the driver's structured, **secret-free** error taxonomy (RFD-0001 §5/§6).
//!
//! Every variant carries machine-facing detail only (op, status, path, a reason string, a Slack
//! `error` code) — **never** the bot token, an `Authorization` header value, the signing secret,
//! or any credential. The reason strings are built from the request *shape*, so a token cannot
//! reach them by construction (the planted-canary test asserts this).

/// Why a Slack operation failed — the per-call error the driver surfaces. Secret-free: constructed
/// from the request shape, the response status, and Slack's own `error` string (a code, never a
/// credential).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SlackError {
    /// The path did not resolve to a Slack node (not under the mount, malformed sub-path).
    #[error("invalid Slack path {path:?}: {reason}")]
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

    /// A `CALL slack.<proc>` named a procedure this driver does not declare.
    #[error("unknown procedure: {0}")]
    UnknownProcedure(String),

    /// A non-2xx Slack HTTP status. Carries the op + status — **never** the auth header.
    #[error("Slack API {op} returned HTTP status {status}")]
    Http {
        /// The op label (e.g. `chat.postMessage`).
        op: &'static str,
        /// The HTTP status code.
        status: u16,
    },

    /// **The t18 BodyErrorRule carry-over (the reason t25 consumes it).** Slack returns HTTP 200
    /// with a JSON envelope `{"ok":false,"error":"<code>"}`; the seam maps that to this structured
    /// **terminal** error carrying Slack's `error` code (e.g. `channel_not_found`,
    /// `not_in_channel`) — a code, never a credential. Default-off / opt-in on the config (t18);
    /// Slack turns it on.
    #[error("Slack API {op} returned ok=false: {code}")]
    Body {
        /// The op label.
        op: &'static str,
        /// Slack's machine-facing `error` code from the body (never a credential).
        code: String,
    },

    /// A transport failure before a status was received (DNS, connect, TLS, timeout). Carries the
    /// op + a secret-free class reason (the transport's class, never a header value).
    #[error("Slack API {op} transport error: {reason}")]
    Transport {
        /// The op label.
        op: &'static str,
        /// A secret-free transport class reason.
        reason: String,
    },

    /// The bot token / signing secret could not be resolved from the credential store. Carries the
    /// secret-free store error code, never the value.
    #[error("auth resolution failed: {code}")]
    Auth {
        /// The secret-free store error code (e.g. `secret_not_found`, `secret_locked`).
        code: &'static str,
    },

    /// A response body could not be decoded into the owned DTO (the body's shape, never a
    /// credential — a Slack JSON body carries no token).
    #[error("decode error for {op}: {reason}")]
    Decode {
        /// The op label.
        op: &'static str,
        /// A secret-free reason.
        reason: String,
    },
}

impl SlackError {
    /// A short, stable machine code for structured surfaces and AI recovery.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SlackError::InvalidPath { .. } => "slack_invalid_path",
            SlackError::MalformedEffect { .. } => "slack_malformed_effect",
            SlackError::CapabilityDenied { .. } => "slack_capability_denied",
            SlackError::UnknownProcedure(_) => "slack_unknown_procedure",
            SlackError::Http { .. } => "slack_http",
            SlackError::Body { .. } => "slack_body_error",
            SlackError::Transport { .. } => "slack_transport",
            SlackError::Auth { .. } => "slack_auth",
            SlackError::Decode { .. } => "slack_decode",
        }
    }

    /// Whether the HTTP status is transient (5xx / 429) — the retry-class gate. A
    /// [`SlackError::Body`] (`ok:false`) is a **terminal** application error, never transient.
    #[must_use]
    pub const fn is_transient_status(status: u16) -> bool {
        status >= 500 || status == 429
    }
}

impl From<crate::client::TransportError> for SlackError {
    /// Map a transport-seam error onto a secret-free [`SlackError`]. Only the transport class
    /// crosses; the seam never carries a header value, so nothing secret can leak.
    fn from(e: crate::client::TransportError) -> Self {
        SlackError::Transport {
            op: "http",
            reason: e.reason,
        }
    }
}
