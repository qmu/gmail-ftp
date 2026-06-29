//! The two discovery documents: [`ProtectedResourceMetadata`] (RFC 9728) and
//! [`AuthorizationServerMetadata`] (RFC 8414).
//!
//! ## Honesty (the t48→t49 sequencing rule)
//! t48 advertised ONLY what was live then — `issuer`, `jwks_uri`, and the static capability arrays —
//! keeping `authorization_endpoint` / `token_endpoint` / `registration_endpoint` `None` (omitted)
//! because advertising an endpoint that 404s would mislead a client. **t49 makes those endpoints
//! live** (DCR + the auth-code/PKCE flow + the token endpoint), so [`AuthorizationServerMetadata::new`]
//! now ALSO advertises them (derived from the issuer origin) and sets `grant_types_supported`
//! including `authorization_code` + `refresh_token`. The fields are still `Option`s (a future
//! deployment could omit one), but the live AS sets all three.

use serde::{Deserialize, Serialize};

use crate::{AUTHORIZE_PATH, REGISTER_PATH, TOKEN_PATH};

/// **RFC 9728** Protected Resource Metadata — served at
/// `/.well-known/oauth-protected-resource`. It tells a client which authorization server(s) guard a
/// protected resource (here, the qfs MCP endpoint), so the client knows where to obtain a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// The protected resource's identifier — the absolute URL of the qfs MCP endpoint.
    pub resource: String,
    /// The authorization server issuer identifier(s) that can issue tokens for `resource`.
    pub authorization_servers: Vec<String>,
    /// How a bearer token is presented to the resource — `header` (the `Authorization` header).
    /// Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bearer_methods_supported: Vec<String>,
}

impl ProtectedResourceMetadata {
    /// Build the PRM for the MCP `resource` guarded by the single authorization-server `issuer`.
    /// Advertises bearer presentation via the `Authorization` header (the MCP transport convention).
    #[must_use]
    pub fn new(resource: impl Into<String>, issuer: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: vec![issuer.into()],
            bearer_methods_supported: vec!["header".to_string()],
        }
    }
}

/// **RFC 8414** Authorization Server Metadata — served at
/// `/.well-known/oauth-authorization-server`. As of t49 the endpoints are LIVE, so the builder
/// advertises `authorization_endpoint` / `token_endpoint` / `registration_endpoint` (derived from
/// the issuer origin) alongside `issuer`, `jwks_uri`, and the static capability arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    /// The authorization server's issuer identifier (its base origin URL). MUST match what the
    /// client used to fetch this document (RFC 8414 §3.3).
    pub issuer: String,
    /// The absolute URL of this AS's JWK Set (`/jwks.json`) — live this milestone.
    pub jwks_uri: String,
    /// The OAuth `response_type` values supported — `["code"]` (the auth-code flow t49 will serve).
    pub response_types_supported: Vec<String>,
    /// The PKCE code-challenge methods supported — `["S256"]` (RFD §10: PKCE is mandatory).
    pub code_challenge_methods_supported: Vec<String>,
    /// The OAuth grant types supported — `authorization_code` + `refresh_token` (live as of t49).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_types_supported: Vec<String>,
    /// The authorization endpoint (the auth-code flow) — live as of t49.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    /// The token endpoint (code→token exchange) — live as of t49.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// The dynamic-client-registration endpoint (RFC 7591) — live as of t49.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
}

impl AuthorizationServerMetadata {
    /// Build the AS metadata for `issuer`, advertising the live `jwks_uri`, the static capability
    /// arrays, AND (as of t49) the authorization / token / registration endpoints derived from the
    /// `issuer` origin, plus the supported grant types. `issuer` is the AS origin (no trailing `/`).
    #[must_use]
    pub fn new(issuer: impl Into<String>, jwks_uri: impl Into<String>) -> Self {
        let issuer = issuer.into();
        Self {
            authorization_endpoint: Some(format!("{issuer}{AUTHORIZE_PATH}")),
            token_endpoint: Some(format!("{issuer}{TOKEN_PATH}")),
            registration_endpoint: Some(format!("{issuer}{REGISTER_PATH}")),
            jwks_uri: jwks_uri.into(),
            response_types_supported: vec!["code".to_string()],
            code_challenge_methods_supported: vec!["S256".to_string()],
            grant_types_supported: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            issuer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "http://localhost:8787";

    #[test]
    fn prm_golden_shape_points_at_the_issuer() {
        let prm = ProtectedResourceMetadata::new(format!("{ISSUER}/mcp"), ISSUER);
        let v = serde_json::to_value(&prm).unwrap();
        assert_eq!(v["resource"], "http://localhost:8787/mcp");
        assert_eq!(v["authorization_servers"][0], ISSUER);
        assert_eq!(v["bearer_methods_supported"][0], "header");
        // Exactly the three members.
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn as_metadata_golden_shape_advertises_the_live_t49_endpoints() {
        let asm = AuthorizationServerMetadata::new(ISSUER, format!("{ISSUER}/jwks.json"));
        let v = serde_json::to_value(&asm).unwrap();
        assert_eq!(v["issuer"], ISSUER);
        assert_eq!(v["jwks_uri"], "http://localhost:8787/jwks.json");
        assert_eq!(v["response_types_supported"][0], "code");
        assert_eq!(v["code_challenge_methods_supported"][0], "S256");
        // t49: the flow endpoints are LIVE now, so they ARE advertised (derived from the issuer).
        assert_eq!(
            v["authorization_endpoint"],
            "http://localhost:8787/authorize"
        );
        assert_eq!(v["token_endpoint"], "http://localhost:8787/token");
        assert_eq!(v["registration_endpoint"], "http://localhost:8787/register");
        // PKCE S256 stays mandatory; the grant types include authorization_code.
        assert_eq!(
            v["code_challenge_methods_supported"],
            serde_json::json!(["S256"])
        );
        assert_eq!(v["grant_types_supported"][0], "authorization_code");
        assert!(v["grant_types_supported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g == "refresh_token"));
        // The full live member set (4 static + 3 endpoints + grant_types).
        assert_eq!(v.as_object().unwrap().len(), 8);
    }

    #[test]
    fn as_metadata_round_trips_and_t49_can_set_endpoints() {
        let mut asm = AuthorizationServerMetadata::new(ISSUER, format!("{ISSUER}/jwks.json"));
        // The t49 seam: setting the endpoint makes it appear in the document.
        asm.token_endpoint = Some(format!("{ISSUER}/oauth/token"));
        let v = serde_json::to_value(&asm).unwrap();
        assert_eq!(v["token_endpoint"], "http://localhost:8787/oauth/token");
        let back: AuthorizationServerMetadata = serde_json::from_value(v).unwrap();
        assert_eq!(asm, back);
    }
}
