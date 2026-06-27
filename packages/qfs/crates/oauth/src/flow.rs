//! The pure **authorization-code + PKCE** protocol logic: authorization-request validation, the
//! exact-match `redirect_uri` check, token-request validation, the OAuth error taxonomy, and the
//! signed access-token claim set. Storage (the code/refresh rows) and HTTP (the routes, the consent
//! HTML) are injected by the binary; this stays pure + unit-testable.
//!
//! ## Security invariants this module pins (RFD §10)
//! - **`response_type` is `code`** only (the authorization-code grant).
//! - **PKCE `S256` is mandatory** — a request without a `code_challenge`, or with a non-`S256`
//!   method, is rejected ([`crate::pkce`] does the `plain`-refusal at the verify step too).
//! - **`state` is required** so the client can defend against CSRF on the redirect.
//! - **`redirect_uri` is EXACT-matched** against the client's registered allowlist
//!   ([`redirect_uri_is_registered`]) — never a prefix/substring match (open-redirect defense).
//! - **`grant_type` is `authorization_code`** at the token endpoint; the access token binds
//!   `iss`/`aud`/`sub`/`scope`/`exp` so a resource server (t50) can verify + scope it.

use serde::Serialize;
use serde_json::{json, Value};

use crate::pkce::PKCE_METHOD_S256;

/// The OAuth error taxonomy (RFC 6749 §4.1.2.1 / §5.2). Each variant maps to a registered `error`
/// code rendered into the redirect query (authorize) or the JSON body (token). Value-free — it names
/// the failing condition, never a code/secret/verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OAuthFlowError {
    /// The request is missing a required parameter or is otherwise malformed.
    #[error("the request is missing a required parameter or is malformed")]
    InvalidRequest,
    /// Client authentication failed / the `client_id` is unknown.
    #[error("client authentication failed or the client is unknown")]
    InvalidClient,
    /// The authorization code is invalid, expired, already used, or does not match this client /
    /// redirect / PKCE verifier.
    #[error("the authorization grant is invalid, expired, or already used")]
    InvalidGrant,
    /// The client is not authorized to use this grant type.
    #[error("the client is not authorized to use this grant type")]
    UnauthorizedClient,
    /// The `grant_type` is not `authorization_code`.
    #[error("the grant type is not supported by the authorization server")]
    UnsupportedGrantType,
    /// The `response_type` is not `code`.
    #[error("the response type is not supported by the authorization server")]
    UnsupportedResponseType,
    /// The requested scope is invalid / unknown.
    #[error("the requested scope is invalid or unknown")]
    InvalidScope,
    /// The resource owner (or AS) denied the request.
    #[error("the resource owner or authorization server denied the request")]
    AccessDenied,
    /// The AS hit an internal error fulfilling the request.
    #[error("the authorization server encountered an unexpected condition")]
    ServerError,
}

impl OAuthFlowError {
    /// The registered OAuth `error` code (the value of the `error` field / query param).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            OAuthFlowError::InvalidRequest => "invalid_request",
            OAuthFlowError::InvalidClient => "invalid_client",
            OAuthFlowError::InvalidGrant => "invalid_grant",
            OAuthFlowError::UnauthorizedClient => "unauthorized_client",
            OAuthFlowError::UnsupportedGrantType => "unsupported_grant_type",
            OAuthFlowError::UnsupportedResponseType => "unsupported_response_type",
            OAuthFlowError::InvalidScope => "invalid_scope",
            OAuthFlowError::AccessDenied => "access_denied",
            OAuthFlowError::ServerError => "server_error",
        }
    }
}

/// A parsed `GET /authorize` request (the query parameters that drive the auth-code flow). Owned +
/// already-extracted from the wire so validation is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeRequest {
    /// `response_type` — MUST be `code`.
    pub response_type: String,
    /// The registered client's id.
    pub client_id: String,
    /// The redirect URI to return the code to — exact-matched against the registered allowlist.
    pub redirect_uri: String,
    /// The requested scope (space-delimited; may be empty).
    pub scope: String,
    /// The opaque CSRF `state` the client echoes — REQUIRED.
    pub state: String,
    /// The PKCE `code_challenge` — REQUIRED.
    pub code_challenge: String,
    /// The PKCE method — MUST be `S256`.
    pub code_challenge_method: String,
}

/// Whether `requested` is EXACTLY one of the client's `registered` redirect URIs. Exact byte match —
/// never a prefix/substring/normalization match (open-redirect defense, RFD §10). A code is issued
/// only to a registered URI, so a mismatch must NOT be redirected (the binary renders an error page).
#[must_use]
pub fn redirect_uri_is_registered(requested: &str, registered: &[String]) -> bool {
    registered.iter().any(|u| u == requested)
}

/// Validate an authorization request's PROTOCOL parameters (everything EXCEPT the client/redirect
/// lookup, which the binary does against the store first so a bad redirect is never redirected to).
/// Checks `response_type=code`, a present `state`, a present `code_challenge`, and
/// `code_challenge_method=S256` (mandatory PKCE; `plain`/missing rejected).
///
/// # Errors
/// The most specific [`OAuthFlowError`] for the first failing check.
pub fn validate_authorize_request(req: &AuthorizeRequest) -> Result<(), OAuthFlowError> {
    if req.response_type != "code" {
        return Err(OAuthFlowError::UnsupportedResponseType);
    }
    if req.state.is_empty() {
        return Err(OAuthFlowError::InvalidRequest);
    }
    if req.code_challenge.is_empty() {
        return Err(OAuthFlowError::InvalidRequest);
    }
    // PKCE S256 is mandatory: a missing or non-S256 method is refused (no `plain` downgrade).
    if req.code_challenge_method != PKCE_METHOD_S256 {
        return Err(OAuthFlowError::InvalidRequest);
    }
    Ok(())
}

/// A parsed `POST /token` request body (the `application/x-www-form-urlencoded` parameters of the
/// authorization-code exchange). Owned so validation is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequest {
    /// `grant_type` — MUST be `authorization_code`.
    pub grant_type: String,
    /// The authorization code to exchange.
    pub code: String,
    /// The redirect URI the code was issued to (re-checked against the code's bound URI).
    pub redirect_uri: String,
    /// The client id presented (a public client identifies itself here).
    pub client_id: String,
    /// The PKCE `code_verifier` — checked against the code's stored `code_challenge`.
    pub code_verifier: String,
}

/// The fixed grant type the token endpoint serves.
pub const GRANT_AUTHORIZATION_CODE: &str = "authorization_code";

/// Validate a token request's SHAPE (the required parameters are present + the grant type is
/// supported) before the binary verifies the code + PKCE against the store. A missing `code` /
/// `code_verifier` / `client_id` is an `invalid_request`; a wrong `grant_type` is
/// `unsupported_grant_type`.
///
/// # Errors
/// The most specific [`OAuthFlowError`] for the first failing check.
pub fn validate_token_request(req: &TokenRequest) -> Result<(), OAuthFlowError> {
    if req.grant_type != GRANT_AUTHORIZATION_CODE {
        return Err(OAuthFlowError::UnsupportedGrantType);
    }
    if req.code.is_empty() || req.client_id.is_empty() || req.code_verifier.is_empty() {
        return Err(OAuthFlowError::InvalidRequest);
    }
    Ok(())
}

/// A parsed `POST /token` request body for the **refresh-token grant** (RFC 6749 §6 / OAuth 2.1 §4.3).
/// Owned so validation is pure. The presented `refresh_token` is the opaque handle the client received
/// at the prior exchange — the binary looks it up by its HASH (never the plaintext) and rotates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshTokenRequest {
    /// `grant_type` — MUST be `refresh_token`.
    pub grant_type: String,
    /// The opaque refresh-token handle to redeem (hashed-looked-up + rotated by the binary).
    pub refresh_token: String,
    /// The client id presented (re-checked against the stored handle's bound client).
    pub client_id: String,
}

/// The refresh-token grant type the token endpoint also serves (single-use rotation, t50).
pub const GRANT_REFRESH_TOKEN: &str = "refresh_token";

/// Validate a refresh-token request's SHAPE (required parameters present + grant type is the refresh
/// grant) before the binary looks the handle up against the store. A missing `refresh_token` /
/// `client_id` is an `invalid_request`; a wrong `grant_type` is `unsupported_grant_type`. The handle
/// itself is validated (hashed lookup, expiry, single-use) by the binary's store, not here.
///
/// # Errors
/// The most specific [`OAuthFlowError`] for the first failing check.
pub fn validate_refresh_request(req: &RefreshTokenRequest) -> Result<(), OAuthFlowError> {
    if req.grant_type != GRANT_REFRESH_TOKEN {
        return Err(OAuthFlowError::UnsupportedGrantType);
    }
    if req.refresh_token.is_empty() || req.client_id.is_empty() {
        return Err(OAuthFlowError::InvalidRequest);
    }
    Ok(())
}

/// Build the signed access-token claim set (RFC 9068-style): `iss` (the AS issuer), `aud` (the MCP
/// resource the token is good for), `sub` (the authenticated user id), `scope`, `client_id`, and the
/// `iat`/`exp` window (`now_unix` .. `now_unix + ttl_secs`). The binary signs this via
/// [`crate::sign_jws`] with the active ES256 key. NEVER carries a code/verifier/secret.
#[must_use]
pub fn access_token_claims(
    issuer: &str,
    audience: &str,
    subject_user_id: i64,
    scope: &str,
    client_id: &str,
    now_unix: u64,
    ttl_secs: u64,
) -> Value {
    json!({
        "iss": issuer,
        "aud": audience,
        "sub": subject_user_id.to_string(),
        "scope": scope,
        "client_id": client_id,
        "iat": now_unix,
        "exp": now_unix + ttl_secs,
    })
}

/// The successful token-endpoint response (RFC 6749 §5.1). `Serialize`-only; the access + refresh
/// token strings are placed here for the single JSON delivery and never logged.
#[derive(Debug, Clone, Serialize)]
pub struct TokenResponse {
    /// The signed ES256 JWS access token.
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// The access-token lifetime in seconds.
    pub expires_in: u64,
    /// The opaque refresh-token handle (stored hashed at rest; enforced/refreshed in t50).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The granted scope, echoed back.
    #[serde(skip_serializing_if = "str::is_empty")]
    pub scope: String,
}

impl TokenResponse {
    /// Build a `Bearer` token response.
    #[must_use]
    pub fn bearer(
        access_token: String,
        expires_in: u64,
        refresh_token: Option<String>,
        scope: String,
    ) -> Self {
        Self {
            access_token,
            token_type: "Bearer".to_string(),
            expires_in,
            refresh_token,
            scope,
        }
    }
}

/// Render an OAuth error as the JSON body of a token-endpoint failure (`{"error":..,
/// "error_description":..}`, RFC 6749 §5.2). The description is a fixed, secret-free sentence.
#[must_use]
pub fn error_json(err: OAuthFlowError) -> Value {
    json!({
        "error": err.code(),
        "error_description": err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_authorize() -> AuthorizeRequest {
        AuthorizeRequest {
            response_type: "code".to_string(),
            client_id: "client-1".to_string(),
            redirect_uri: "https://app.example/cb".to_string(),
            scope: "mcp:read".to_string(),
            state: "xyz-state".to_string(),
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_string(),
            code_challenge_method: "S256".to_string(),
        }
    }

    #[test]
    fn redirect_uri_match_is_exact_not_prefix() {
        let registered = vec!["https://app.example/cb".to_string()];
        assert!(redirect_uri_is_registered(
            "https://app.example/cb",
            &registered
        ));
        // A prefix / substring / extra-path must NOT match (open-redirect defense).
        assert!(!redirect_uri_is_registered(
            "https://app.example/cb/evil",
            &registered
        ));
        assert!(!redirect_uri_is_registered(
            "https://app.example",
            &registered
        ));
        assert!(!redirect_uri_is_registered(
            "https://evil.example/cb",
            &registered
        ));
    }

    #[test]
    fn a_valid_authorize_request_passes() {
        assert!(validate_authorize_request(&good_authorize()).is_ok());
    }

    #[test]
    fn a_non_code_response_type_is_unsupported() {
        let mut r = good_authorize();
        r.response_type = "token".to_string();
        assert_eq!(
            validate_authorize_request(&r).unwrap_err(),
            OAuthFlowError::UnsupportedResponseType
        );
    }

    #[test]
    fn pkce_is_mandatory_and_plain_is_rejected() {
        // Missing challenge.
        let mut r = good_authorize();
        r.code_challenge = String::new();
        assert_eq!(
            validate_authorize_request(&r).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );
        // `plain` method is refused (no downgrade).
        let mut r = good_authorize();
        r.code_challenge_method = "plain".to_string();
        assert_eq!(
            validate_authorize_request(&r).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );
    }

    #[test]
    fn state_is_required() {
        let mut r = good_authorize();
        r.state = String::new();
        assert_eq!(
            validate_authorize_request(&r).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );
    }

    #[test]
    fn token_request_requires_the_auth_code_grant_and_all_params() {
        let good = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: "abc".to_string(),
            redirect_uri: "https://app.example/cb".to_string(),
            client_id: "client-1".to_string(),
            code_verifier: "verifier".to_string(),
        };
        assert!(validate_token_request(&good).is_ok());

        let mut bad = good.clone();
        bad.grant_type = "client_credentials".to_string();
        assert_eq!(
            validate_token_request(&bad).unwrap_err(),
            OAuthFlowError::UnsupportedGrantType
        );

        let mut bad = good.clone();
        bad.code_verifier = String::new();
        assert_eq!(
            validate_token_request(&bad).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );
    }

    #[test]
    fn access_token_claims_carry_the_binding_and_window() {
        let claims = access_token_claims(
            "http://localhost:8787",
            "http://localhost:8787/mcp",
            42,
            "mcp:read",
            "client-1",
            1_000,
            3_600,
        );
        assert_eq!(claims["iss"], "http://localhost:8787");
        assert_eq!(claims["aud"], "http://localhost:8787/mcp");
        assert_eq!(claims["sub"], "42");
        assert_eq!(claims["scope"], "mcp:read");
        assert_eq!(claims["client_id"], "client-1");
        assert_eq!(claims["iat"], 1_000);
        assert_eq!(claims["exp"], 4_600);
    }

    #[test]
    fn refresh_request_requires_the_refresh_grant_and_all_params() {
        let good = RefreshTokenRequest {
            grant_type: "refresh_token".to_string(),
            refresh_token: "opaque-handle".to_string(),
            client_id: "client-1".to_string(),
        };
        assert!(validate_refresh_request(&good).is_ok());

        let mut bad = good.clone();
        bad.grant_type = "authorization_code".to_string();
        assert_eq!(
            validate_refresh_request(&bad).unwrap_err(),
            OAuthFlowError::UnsupportedGrantType
        );

        let mut bad = good.clone();
        bad.refresh_token = String::new();
        assert_eq!(
            validate_refresh_request(&bad).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );

        let mut bad = good;
        bad.client_id = String::new();
        assert_eq!(
            validate_refresh_request(&bad).unwrap_err(),
            OAuthFlowError::InvalidRequest
        );
    }

    #[test]
    fn error_json_has_the_oauth_shape() {
        let v = error_json(OAuthFlowError::InvalidGrant);
        assert_eq!(v["error"], "invalid_grant");
        assert!(v["error_description"].as_str().unwrap().len() > 5);
    }

    #[test]
    fn token_response_omits_an_absent_refresh_token() {
        let r = TokenResponse::bearer("jws".to_string(), 600, None, String::new());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["token_type"], "Bearer");
        assert_eq!(v["expires_in"], 600);
        assert!(v.get("refresh_token").is_none());
        assert!(v.get("scope").is_none(), "empty scope is omitted");
    }
}
