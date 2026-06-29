//! Dynamic client registration (**RFC 7591**) — the pure request validation + response DTOs.
//!
//! An MCP client `POST`s a registration request (its `redirect_uris`, an optional `client_name`) and
//! the AS mints a public `client_id` for it. qfs registers **public PKCE clients** — there is NO
//! client secret (PKCE is the proof-of-possession), so `token_endpoint_auth_method` is `none`. This
//! module owns the pure half: validating the request (the `redirect_uris` are present and well-formed
//! absolute http/https URLs with no fragment — the exact allowlist a `redirect_uri` is later
//! exact-matched against, RFD §10 open-redirect defense) and shaping the RFC 7591 response. Minting
//! the `client_id` + persisting the row is the binary-injected store layer.
//!
//! **OPEN PRODUCT DECISION (flagged for the reviewer, not baked in):** DCR is **open** here (the MCP
//! norm — a client self-registers with no operator step). That invites client-row spam; a later
//! ticket may add a soft cap / per-client expiry / a gate. The redirect-URI allowlist + mandatory
//! PKCE bound the blast radius of an open registration for now.

use serde::{Deserialize, Serialize};

/// The `token_endpoint_auth_method` qfs registers every client with: `none` — a public PKCE client
/// authenticates the token exchange with the PKCE verifier, not a client secret.
pub const AUTH_METHOD_NONE: &str = "none";

/// A dynamic client registration request (RFC 7591 §2/§3.1). Only the members qfs acts on are
/// modeled; unknown members are ignored (forward-compatible). `redirect_uris` is REQUIRED for the
/// authorization-code grant.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRegistrationRequest {
    /// The client's redirect URIs — the exact allowlist `redirect_uri` is matched against. Required,
    /// non-empty, each a well-formed absolute http/https URL with no fragment.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    /// An optional human-readable client name, echoed back in the response.
    #[serde(default)]
    pub client_name: Option<String>,
}

/// A dynamic client registration response (RFC 7591 §3.2.1). Carries the minted `client_id`, the
/// registered metadata echoed back, and the fixed public-PKCE-client capability set. There is NO
/// `client_secret` member — qfs issues public clients only.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRegistrationResponse {
    /// The minted public client identifier.
    pub client_id: String,
    /// When the `client_id` was issued (seconds since the Unix epoch).
    pub client_id_issued_at: u64,
    /// The registered redirect URIs (the exact allowlist), echoed back.
    pub redirect_uris: Vec<String>,
    /// Always `none` — a public PKCE client (no secret).
    pub token_endpoint_auth_method: String,
    /// The grant types this client may use — `["authorization_code"]` (+ `refresh_token`).
    pub grant_types: Vec<String>,
    /// The response types this client may request — `["code"]`.
    pub response_types: Vec<String>,
    /// The optional client name, echoed back when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl ClientRegistrationResponse {
    /// Build the public-PKCE-client response for a freshly minted `client_id` registered against
    /// `redirect_uris`, issued at `issued_at` (Unix seconds).
    #[must_use]
    pub fn public_client(
        client_id: impl Into<String>,
        redirect_uris: Vec<String>,
        client_name: Option<String>,
        issued_at: u64,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_id_issued_at: issued_at,
            redirect_uris,
            token_endpoint_auth_method: AUTH_METHOD_NONE.to_string(),
            grant_types: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            response_types: vec!["code".to_string()],
            client_name,
        }
    }
}

/// Why a registration request was rejected (RFC 7591 §3.2.2 maps these to the `invalid_redirect_uri`
/// / `invalid_client_metadata` error codes). Value-free + AI-consumable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    /// No `redirect_uris` were supplied (required for the authorization-code grant).
    #[error("at least one redirect_uri is required")]
    MissingRedirectUris,
    /// A `redirect_uri` was not a well-formed absolute http/https URL, or carried a fragment.
    #[error("a redirect_uri is not a valid absolute http(s) URI without a fragment")]
    InvalidRedirectUri,
}

impl RegistrationError {
    /// The RFC 7591 §3.2.2 error code for the registration-error response body.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            RegistrationError::MissingRedirectUris | RegistrationError::InvalidRedirectUri => {
                "invalid_redirect_uri"
            }
        }
    }
}

/// Validate a registration request's `redirect_uris`: at least one, each a well-formed absolute
/// http/https URL with an authority and NO fragment (a fragment in a redirect URI is forbidden by
/// RFC 6749 §3.1.2 and is an open-redirect smuggling vector).
///
/// # Errors
/// [`RegistrationError`] naming the first problem found.
pub fn validate_registration(req: &ClientRegistrationRequest) -> Result<(), RegistrationError> {
    if req.redirect_uris.is_empty() {
        return Err(RegistrationError::MissingRedirectUris);
    }
    for uri in &req.redirect_uris {
        if !is_valid_redirect_uri(uri) {
            return Err(RegistrationError::InvalidRedirectUri);
        }
    }
    Ok(())
}

/// Whether `uri` is a well-formed absolute redirect URI for registration: an `http`/`https` scheme,
/// a non-empty authority, and NO `#` fragment. Deliberately strict (no custom schemes, no
/// fragments) — the registered set becomes the exact allowlist, so a loose entry here is a standing
/// open-redirect risk.
fn is_valid_redirect_uri(uri: &str) -> bool {
    if uri.contains('#') {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // The authority is everything up to the first '/', '?', or end — it must be non-empty.
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    !authority.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(uris: &[&str]) -> ClientRegistrationRequest {
        ClientRegistrationRequest {
            redirect_uris: uris.iter().map(|s| (*s).to_string()).collect(),
            client_name: None,
        }
    }

    #[test]
    fn a_well_formed_registration_validates() {
        assert!(validate_registration(&req(&[
            "https://app.example/callback",
            "http://localhost:6274/oauth/callback"
        ]))
        .is_ok());
    }

    #[test]
    fn missing_redirect_uris_is_rejected() {
        let err = validate_registration(&req(&[])).unwrap_err();
        assert_eq!(err, RegistrationError::MissingRedirectUris);
        assert_eq!(err.code(), "invalid_redirect_uri");
    }

    #[test]
    fn malformed_or_fragment_redirect_uris_are_rejected() {
        for bad in [
            "not-a-url",
            "ftp://app.example/cb",        // wrong scheme
            "https://app.example/cb#frag", // fragment forbidden
            "https://",                    // empty authority
            "/relative/only",
        ] {
            assert_eq!(
                validate_registration(&req(&[bad])).unwrap_err(),
                RegistrationError::InvalidRedirectUri,
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_request_deserializes_from_the_rfc7591_json_shape() {
        let json = r#"{"redirect_uris":["https://app.example/cb"],"client_name":"My MCP App","scope":"mcp:read"}"#;
        let req: ClientRegistrationRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.redirect_uris, vec!["https://app.example/cb"]);
        assert_eq!(req.client_name.as_deref(), Some("My MCP App"));
    }

    #[test]
    fn the_response_is_a_public_client_with_no_secret() {
        let resp = ClientRegistrationResponse::public_client(
            "client-abc",
            vec!["https://app.example/cb".to_string()],
            Some("My App".to_string()),
            1_700_000_000,
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["client_id"], "client-abc");
        assert_eq!(v["token_endpoint_auth_method"], "none");
        assert_eq!(v["grant_types"][0], "authorization_code");
        assert_eq!(v["response_types"][0], "code");
        // A public client carries NO secret.
        assert!(
            v.get("client_secret").is_none(),
            "public clients have no secret"
        );
    }
}
