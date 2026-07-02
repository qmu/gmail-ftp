//! [`OAuthClient`] — the thin Google OAuth2 endpoints client (RFD-0001 §5/§171): builds the
//! consent/auth URL, exchanges an authorization code for tokens, refreshes an access token,
//! and looks up the profile email. **No vendor SDK** — owned DTOs + the thin [`HttpExchange`]
//! seam only (`reqwest` stays confined to `qfs-driver-http`, behind the consuming driver's
//! adapter).
//!
//! ## The load-bearing redirect detail (RFD §10, the hard part)
//! The redirect URI host is **`localhost`**, never `127.0.0.1`. Desktop OAuth clients stall on
//! Google's silent-consent path when the redirect host is the loopback *IP*; `http://localhost`
//! completes reliably. The paste-back flow ([`crate::authorize`]) advertises the portless
//! [`PASTE_REDIRECT_URI`] — nothing ever listens at it; the user pastes the redirect their
//! browser lands on back into the terminal — and a unit test asserts the auth URL carries
//! `localhost`, not the IP.
//!
//! ## Secret discipline (RFD §10)
//! `client_secret`, the authorization `code`, the refresh token, and the minted access token
//! are all [`Secret`] or move straight onto the wire form body via `expose` — none enters a
//! struct field that is logged, an error `Display`, or a `Debug`. The request the token
//! endpoint sees rides the redacting [`HttpRequest`] (its `Authorization`/secret form body is
//! never the URL); we additionally keep the form *body* off every log surface here.

use std::time::Duration;

use qfs_secrets::Secret;
use serde::Deserialize;

use crate::error::AuthError;
use crate::http::{HttpExchange, HttpMethod, HttpRequest, HttpResponse};
use crate::token::{AccessToken, DEFAULT_EXPIRY_SKEW};

/// Google's OAuth2 authorization endpoint (the consent URL base).
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's OAuth2 token endpoint (code exchange + refresh).
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Google's OIDC userinfo endpoint (profile email lookup).
pub const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/userinfo";

/// The paste-back consent redirect URI — `http://localhost`, portless, with NO listener behind
/// it. The host is `localhost`, NOT `127.0.0.1` (the load-bearing detail: Desktop OAuth clients
/// stall on silent consent when the redirect host is the loopback IP). The user's browser lands
/// on this URL after approving — it fails to load, by design — and the user pastes it back into
/// the terminal. The token exchange must send this SAME value as `redirect_uri` (Google checks
/// equality, not reachability). Pinned as a constant so a test can assert it.
pub const PASTE_REDIRECT_URI: &str = "http://localhost";

/// The thin Google OAuth2 endpoints client. Holds the desktop client credentials and the
/// caller-supplied scope set (this crate is **scope-agnostic** — Gmail/Drive/Analytics each
/// pass their minimum scopes); performs the token exchange/refresh + userinfo over the
/// injected [`HttpExchange`].
///
/// `client_secret` is a [`Secret`]; it is exposed only when writing the token form body and is
/// never stored in a logged field or surfaced in an error.
pub struct OAuthClient {
    client_id: String,
    client_secret: Secret,
    scopes: Vec<String>,
    http: std::sync::Arc<dyn HttpExchange>,
    skew: Duration,
}

impl OAuthClient {
    /// Build an OAuth client for a desktop OAuth app. `scopes` is the consuming driver's
    /// minimum scope set (least privilege, RFD §10). Network rides `http`.
    #[must_use]
    pub fn new(
        client_id: impl Into<String>,
        client_secret: Secret,
        scopes: Vec<String>,
        http: std::sync::Arc<dyn HttpExchange>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            scopes,
            http,
            skew: DEFAULT_EXPIRY_SKEW,
        }
    }

    /// Override the access-token refresh skew (test seam / tuning). Default
    /// [`DEFAULT_EXPIRY_SKEW`].
    #[must_use]
    pub fn with_skew(mut self, skew: Duration) -> Self {
        self.skew = skew;
        self
    }

    /// The caller's scope set (least-privilege, per consuming driver).
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// The non-secret OAuth client id.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Build the consent/authorization URL for the paste-back flow. Sets
    /// `access_type=offline` + `prompt=consent` so Google **reliably** returns a refresh token
    /// (it otherwise omits it on re-consent), `include_granted_scopes=true` so a later consent
    /// **accumulates** onto scopes the user already granted this client (Google's incremental
    /// authorization) instead of replacing them — the single shared `google:<email>:refresh_token`
    /// then covers gmail + drive + analytics even when each driver consents with its own narrow
    /// scope set — and threads `state` (CSRF) and the `redirect_uri` ([`PASTE_REDIRECT_URI`]).
    /// The user opens this URL; on approval Google redirects to `redirect_uri?code=...&state=...`.
    ///
    /// # Errors
    /// [`AuthError::Invalid`] if the base auth endpoint cannot be parsed as a URL.
    pub fn build_auth_url(&self, redirect_uri: &str, state: &str) -> Result<String, AuthError> {
        let mut url = url::Url::parse(AUTH_ENDPOINT).map_err(|_| AuthError::Invalid {
            reason: "auth endpoint is not a valid URL".to_string(),
        })?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("include_granted_scopes", "true")
            .append_pair("state", state);
        Ok(url.into())
    }

    /// Exchange an authorization `code` (from the pasted redirect) for tokens, returning the
    /// minted [`AccessToken`] and the long-lived refresh token. `redirect_uri` must equal the
    /// one in the auth URL ([`PASTE_REDIRECT_URI`]). `now_nanos` anchors expiry on the caller's
    /// clock timeline.
    ///
    /// # Errors
    /// - [`AuthError::Network`] on transport failure.
    /// - [`AuthError::TokenRefresh`] on a non-2xx status or an OAuth `error` body.
    /// - [`AuthError::ProfileLookup`]/[`AuthError::Invalid`] on a malformed token body
    ///   (missing `access_token`/`refresh_token`).
    pub fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        now_nanos: u128,
    ) -> Result<(AccessToken, Secret), AuthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", self.client_id.as_str()),
        ];
        let body = self.encode_token_form(&form);
        let resp = self.post_token(body)?;
        let parsed = parse_token_response(&resp)?;
        let access = self.access_token_from(&parsed, now_nanos)?;
        let refresh = parsed.refresh_token.ok_or_else(|| AuthError::Invalid {
            reason: "token response carried no refresh_token (need access_type=offline + \
                     prompt=consent)"
                .to_string(),
        })?;
        tracing::debug!(grant = "authorization_code", "token exchange succeeded");
        Ok((access, Secret::from(refresh)))
    }

    /// Refresh an access token from a stored refresh token. The refresh token is read from the
    /// [`Secret`] only to place it on the wire form; it never enters a log or an error.
    ///
    /// # Errors
    /// - [`AuthError::Network`] on transport failure.
    /// - [`AuthError::TokenRefresh`] (with `reason == "invalid_grant"`) when the refresh token
    ///   is revoked/expired — the caller must re-`authorize` rather than retry.
    pub fn refresh_access_token(
        &self,
        refresh_token: &Secret,
        now_nanos: u128,
    ) -> Result<AccessToken, AuthError> {
        let rt = refresh_token
            .expose_str()
            .ok_or_else(|| AuthError::Invalid {
                reason: "stored refresh token is not valid UTF-8".to_string(),
            })?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("client_id", self.client_id.as_str()),
        ];
        let body = self.encode_token_form(&form);
        let resp = self.post_token(body)?;
        let parsed = parse_token_response(&resp)?;
        let access = self.access_token_from(&parsed, now_nanos)?;
        tracing::debug!(grant = "refresh_token", "access token refreshed");
        Ok(access)
    }

    /// Look up the authenticated profile **email** via the OIDC userinfo endpoint, using a live
    /// access token as the bearer. Returns an owned `String`; the vendor `Userinfo` shape never
    /// escapes. This keys the account on `authorize`.
    ///
    /// # Errors
    /// [`AuthError::Network`] on transport failure; [`AuthError::ProfileLookup`] on a non-2xx
    /// status, a non-JSON body, or a missing `email` field.
    pub fn fetch_profile_email(&self, access: &AccessToken) -> Result<String, AuthError> {
        let bearer = access.bearer().ok_or_else(|| AuthError::ProfileLookup {
            reason: "access token is not valid UTF-8".to_string(),
        })?;
        let req = HttpRequest::new(HttpMethod::Get, USERINFO_ENDPOINT)
            .header("Authorization", format!("Bearer {bearer}"));
        let resp = self
            .http
            .exchange(&req)
            .map_err(|e| AuthError::network("userinfo", &e))?;
        if !resp.is_success() {
            return Err(AuthError::ProfileLookup {
                reason: format!("userinfo status {}", resp.status),
            });
        }
        #[derive(Deserialize)]
        struct Userinfo {
            email: Option<String>,
        }
        let info: Userinfo =
            serde_json::from_slice(&resp.body).map_err(|_| AuthError::ProfileLookup {
                reason: "userinfo body was not valid JSON".to_string(),
            })?;
        info.email.ok_or_else(|| AuthError::ProfileLookup {
            reason: "userinfo response carried no email".to_string(),
        })
    }

    /// POST a `application/x-www-form-urlencoded` body to the token endpoint, classifying a
    /// non-2xx into a typed [`AuthError`] from the OAuth `error` body (so `invalid_grant`
    /// surfaces as a re-authorize signal). The form `body` carries `client_secret` + the
    /// code/refresh token, so it is **never** logged — only the endpoint + resulting status is.
    fn post_token(&self, body: Vec<u8>) -> Result<HttpResponse, AuthError> {
        let req = HttpRequest::new(HttpMethod::Post, TOKEN_ENDPOINT)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .with_body(body);
        let resp = self
            .http
            .exchange(&req)
            .map_err(|e| AuthError::network("token", &e))?;
        if !resp.is_success() {
            // The token endpoint returns the OAuth error envelope on 4xx (e.g. invalid_grant).
            return Err(token_error_from_body(&resp));
        }
        Ok(resp)
    }

    /// Encode the token form, appending `client_secret` last. The secret is exposed only here,
    /// straight into the form body bytes — it never lands in a struct field or a log.
    fn encode_token_form(&self, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in fields {
            ser.append_pair(k, v);
        }
        if let Some(secret) = self.client_secret.expose_str() {
            ser.append_pair("client_secret", secret);
        }
        ser.finish().into_bytes()
    }

    /// Build an [`AccessToken`] from a parsed token body + the caller's clock reading, applying
    /// the configured refresh skew.
    fn access_token_from(
        &self,
        parsed: &TokenResponse,
        now_nanos: u128,
    ) -> Result<AccessToken, AuthError> {
        let value = parsed
            .access_token
            .as_ref()
            .ok_or_else(|| AuthError::Invalid {
                reason: "token response carried no access_token".to_string(),
            })?;
        let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(0));
        Ok(AccessToken::from_lifetime(
            Secret::from(value.clone()),
            now_nanos,
            lifetime,
            self.skew,
        ))
    }
}

/// The owned token-endpoint response DTO (no vendor type). All fields optional so a partial /
/// error body deserializes without panicking; the caller validates required fields.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

/// The owned OAuth error envelope (`{"error":"invalid_grant", ...}`) — only the slug is read;
/// `error_description` is intentionally ignored (it can echo request detail, kept off our
/// surfaces).
#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    error: Option<String>,
}

/// Parse a 2xx token body into the owned DTO.
fn parse_token_response(resp: &HttpResponse) -> Result<TokenResponse, AuthError> {
    serde_json::from_slice(&resp.body).map_err(|_| AuthError::TokenRefresh {
        reason: "token response body was not valid JSON".to_string(),
    })
}

/// Map a non-2xx token response into a typed [`AuthError::TokenRefresh`] using the OAuth
/// `error` slug when present (so `invalid_grant` is recognizable), else a status-class label.
fn token_error_from_body(resp: &HttpResponse) -> AuthError {
    if let Ok(body) = serde_json::from_slice::<OAuthErrorBody>(&resp.body) {
        if let Some(slug) = body.error {
            return AuthError::TokenRefresh { reason: slug };
        }
    }
    AuthError::TokenRefresh {
        reason: format!("token endpoint status {}", resp.status),
    }
}
