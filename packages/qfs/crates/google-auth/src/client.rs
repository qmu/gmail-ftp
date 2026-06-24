//! [`GoogleApiClient`] — the reusable **authenticated Google API client** the Gmail (t20),
//! Drive (t21), and Analytics (t41) drivers issue their API calls through (RFD-0001 §5/§10).
//!
//! It wraps the thin [`HttpExchange`] seam and a [`TokenSource`]: for each request it injects the
//! current bearer access token into an `Authorization` header, sends it, and — on a **401** —
//! invalidates the cached token, refreshes once, and **retries exactly once**. A second 401 (or
//! a non-401 error) is returned to the driver to classify. This is the "refresh on 401" loop the
//! ticket calls for, factored out so each Google driver reuses it rather than re-implementing.
//!
//! ## Boundary discipline
//! The client trades only in the owned [`HttpRequest`]/[`HttpResponse`] DTOs — the caller builds
//! the request (URL, method, body, non-auth headers); this client owns *only* the
//! `Authorization` header + the refresh-retry. The bearer value is read from the [`Secret`] at
//! send time and written into a header the [`HttpRequest`] `Debug` redacts; it never lands in a
//! log line or an error.

use std::sync::Arc;

use crate::error::AuthError;
use crate::http::{HttpExchange, HttpRequest, HttpResponse};
use crate::source::TokenSource;

/// HTTP 401 — the status that triggers a single token refresh + retry.
const UNAUTHORIZED: u16 = 401;

/// The authenticated Google API client. Construct one per account (it borrows that account's
/// [`TokenSource`]); a driver issues every Google API request through [`GoogleApiClient::send`].
pub struct GoogleApiClient {
    http: Arc<dyn HttpExchange>,
    tokens: Arc<dyn TokenSource>,
}

impl GoogleApiClient {
    /// Build a client sending over `http` and authenticating with `tokens`.
    #[must_use]
    pub fn new(http: Arc<dyn HttpExchange>, tokens: Arc<dyn TokenSource>) -> Self {
        Self { http, tokens }
    }

    /// Send `req` with a bearer access token injected, refreshing once on a 401.
    ///
    /// The caller supplies a request **without** an `Authorization` header (URL + method +
    /// body + any API-specific headers); this method adds the bearer, sends, and — if the
    /// response is 401 — invalidates the token, mints a fresh one, and re-sends exactly once.
    /// Any header named `authorization` already present is dropped first so the bearer is
    /// authoritative and a caller cannot accidentally double-auth.
    ///
    /// # Errors
    /// [`AuthError`] if a token cannot be obtained (store/refresh/network), or the transport
    /// fails. A non-401 HTTP status is **not** an error here — it is returned in the
    /// [`HttpResponse`] for the driver to classify (a 404 body is still decodable).
    pub fn send(&self, req: &HttpRequest) -> Result<HttpResponse, AuthError> {
        let resp = self.send_once(req)?;
        if resp.status != UNAUTHORIZED {
            return Ok(resp);
        }
        // 401: the access token was rejected (revoked/rotated server-side, or a race past our
        // local skew). Drop the cache, refresh once, retry exactly once.
        tracing::debug!("google api returned 401; refreshing token and retrying once");
        self.tokens.invalidate();
        self.send_once(req)
    }

    /// Inject the current bearer and send exactly once.
    fn send_once(&self, req: &HttpRequest) -> Result<HttpResponse, AuthError> {
        let authed = self.with_bearer(req)?;
        self.http
            .exchange(&authed)
            .map_err(|e| AuthError::network("api", &e))
    }

    /// Clone `req`, strip any existing `Authorization` header, and append the current bearer.
    /// The bearer is read from the [`TokenSource`] (which refreshes transparently on expiry).
    fn with_bearer(&self, req: &HttpRequest) -> Result<HttpRequest, AuthError> {
        let token = self.tokens.access_token()?;
        let bearer = token.bearer().ok_or_else(|| AuthError::Invalid {
            reason: "access token is not valid UTF-8".to_string(),
        })?;
        let mut authed = req.clone();
        authed
            .headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
        authed
            .headers
            .push(("Authorization".to_string(), format!("Bearer {bearer}")));
        Ok(authed)
    }
}
