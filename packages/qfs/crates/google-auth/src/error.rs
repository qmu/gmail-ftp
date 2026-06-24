//! [`AuthError`] — the structured, **secret-free** Google-auth error taxonomy
//! (RFD-0001 §5/§10/§103).
//!
//! Every variant carries machine-facing detail only — an endpoint name, an account email
//! (a low-sensitivity identifier), a secret-free store/transport code, or a fixed reason.
//! A `client_secret`, an authorization `code`, an access token, or a refresh token can
//! **never** reach an error's `Display`/`Debug`: the variants take only the shapes above,
//! and the live material lives behind [`qfs_secrets::Secret`], which has no `Display`/
//! `Into<String>`. A redaction test asserts a planted secret never surfaces here.

/// Why a Google-auth operation failed. Secret-free by construction: built from endpoint
/// names, status codes, account emails, and store/transport codes — never a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The user denied consent (the loopback redirect carried `error=access_denied`), or the
    /// authorization request was rejected. Terminal — the user must re-run `authorize` and
    /// approve.
    #[error("authorization denied by the user")]
    Denied,

    /// The loopback consent flow did not complete in time (no redirect was captured before
    /// the deadline). Terminal for this attempt; re-run `authorize`.
    #[error("authorization timed out waiting for the loopback redirect")]
    Timeout,

    /// The loopback redirect arrived but its `state` did not match the value we generated — a
    /// possible CSRF/forgery, so the code is rejected and never exchanged.
    #[error("authorization state mismatch (possible forgery); redirect rejected")]
    StateMismatch,

    /// A network/transport failure reaching a Google endpoint (token exchange, refresh, or
    /// userinfo) before any HTTP status was received. Carries the secret-free transport code
    /// from the t18 HTTP seam.
    #[error("network error reaching {endpoint}: {code}")]
    Network {
        /// Which Google endpoint was being contacted (`token`/`userinfo`), never a URL with
        /// a token in it.
        endpoint: &'static str,
        /// The secret-free transport class code (e.g. `http_transport`).
        code: &'static str,
    },

    /// The token endpoint returned a non-2xx status, or its body was the OAuth `error`
    /// envelope (e.g. `invalid_grant` from a revoked/expired refresh token). The `reason` is
    /// the OAuth `error` slug (a fixed protocol token like `invalid_grant`), never the token.
    /// On `invalid_grant` the caller should surface "re-authorize" rather than loop.
    #[error("token refresh/exchange failed: {reason}")]
    TokenRefresh {
        /// The OAuth `error` slug (`invalid_grant`, `invalid_client`, ...) or a status-class
        /// label. A fixed protocol token, secret-free.
        reason: String,
    },

    /// The userinfo endpoint did not return a usable profile email (missing field, non-2xx,
    /// or a non-JSON body). The account cannot be keyed, so `authorize` fails closed.
    #[error("profile lookup failed: {reason}")]
    ProfileLookup {
        /// A secret-free reason (a status class or a "missing email" note).
        reason: String,
    },

    /// The credential store could not be read/written (no refresh token for the account, the
    /// store is locked, or a backend failure). Carries the secret-free store error *code*
    /// (`secret_not_found`/`secret_locked`/`secret_backend`), never the value.
    #[error("credential store error: {code}")]
    Store {
        /// The secret-free [`qfs_secrets::SecretError`] code.
        code: &'static str,
    },

    /// A configuration/contract error: a malformed account email, an unbuildable auth URL, or
    /// a loopback listener that could not bind. Secret-free reason.
    #[error("invalid auth configuration: {reason}")]
    Invalid {
        /// A secret-free reason.
        reason: String,
    },
}

impl AuthError {
    /// A short, stable machine code for structured surfaces and AI recovery.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            AuthError::Denied => "auth_denied",
            AuthError::Timeout => "auth_timeout",
            AuthError::StateMismatch => "auth_state_mismatch",
            AuthError::Network { .. } => "auth_network",
            AuthError::TokenRefresh { .. } => "auth_token_refresh",
            AuthError::ProfileLookup { .. } => "auth_profile_lookup",
            AuthError::Store { .. } => "auth_store",
            AuthError::Invalid { .. } => "auth_invalid",
        }
    }

    /// Whether this error means "the stored refresh token is no longer usable" — a revoked or
    /// expired refresh grant (`invalid_grant`). The caller surfaces a re-`authorize`
    /// instruction rather than retrying, which would loop forever.
    #[must_use]
    pub fn is_reauthorize_required(&self) -> bool {
        matches!(self, AuthError::TokenRefresh { reason } if reason == "invalid_grant")
    }
}

impl From<qfs_secrets::SecretError> for AuthError {
    fn from(err: qfs_secrets::SecretError) -> Self {
        AuthError::Store { code: err.code() }
    }
}

impl AuthError {
    /// Build an [`AuthError::Network`] tagged with the specific Google `endpoint`, preserving
    /// only the secret-free transport code from the [`crate::http::TransportError`].
    #[must_use]
    pub fn network(endpoint: &'static str, err: &crate::http::TransportError) -> Self {
        AuthError::Network {
            endpoint,
            code: err.code(),
        }
    }
}
