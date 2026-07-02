//! The `qfs serve` **MCP composition** (t47): the binary implements + injects the
//! [`qfs_mcp::McpEngine`] and composes the `POST /mcp` handler into the existing HTTP listener.
//!
//! Like the t32/t33/t34 serve leaves, `qfs-mcp` is a LEAF that consumes `qfs-server` (the policy
//! gate) and `qfs-exec` (the plan/preview) but NOT `qfs-runtime` — so the live half (the
//! describe registry, the real `build_plan`, the runtime-backed `apply`, the redacted connection
//! list) is injected HERE, where the binary already owns the runtime + the credential store. The
//! MCP protocol/tool logic stays pure in `qfs-mcp`; this module is only the wiring.
//!
//! ## The apply runs OFF the serve runtime (no nested `block_on`)
//! [`crate::commit::apply_plan`] builds its own current-thread tokio runtime and `block_on`s the
//! COMMIT interpreter. The MCP handler is invoked synchronously from inside the serve tokio
//! runtime (the listener's request future), and `block_on` panics if called on a runtime thread.
//! So [`ServeMcpEngine::apply`] offloads the commit to a dedicated OS thread (via
//! [`std::thread::scope`]) that owns no ambient runtime — the same "tokio dead-ends in the
//! terminal binary" discipline, just isolated from the listener's reactor.

use std::sync::Arc;

use qfs_core::Engine;
use qfs_mcp::{ConnectionInfo, EngineError, McpEngine};
use qfs_secrets::Secrets;

/// The binary's live [`McpEngine`]: holds the serve engine (mounts + codecs) the plan builder and
/// describe registry resolve against. The describe registry + connection store are built per-call
/// (cred-free / best-effort), exactly as the `qfs describe` and `qfs account list` subcommands do.
pub struct ServeMcpEngine {
    engine: Arc<Engine>,
}

impl ServeMcpEngine {
    /// Build the engine over the shared serve [`Engine`].
    #[must_use]
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

/// Map an executor error into the secret-free [`EngineError`] the MCP surface exposes. ExecError
/// is already secret-free (code + machine-facing message); we carry the stable `kind` as the code
/// and the message verbatim — no path-secret, token, or stack ever crosses this seam.
fn map_exec_err(e: &qfs_exec::ExecError) -> EngineError {
    EngineError::new(e.kind.as_str(), e.message.clone())
}

impl McpEngine for ServeMcpEngine {
    fn describe(&self, path: &str) -> Result<serde_json::Value, EngineError> {
        // The cred-free describe registry — the SAME one `qfs describe` consults (pure: no creds,
        // no I/O, no network; only the introspective driver half is ever touched).
        let registry = crate::describe::describe_registry();
        let (driver, _rest) = registry.resolve_path(path).ok_or_else(|| {
            EngineError::new(
                "unknown_mount",
                format!("no driver is mounted for `{path}` (describe registry)"),
            )
        })?;
        let report =
            qfs_core::DescribeReport::from_driver(driver.as_ref(), &qfs_core::Path::new(path))
                .map_err(|e| map_exec_err(&qfs_exec::map_qfs_error(&e)))?;
        serde_json::to_value(&report)
            .map_err(|e| EngineError::internal(format!("could not render describe report: {e}")))
    }

    fn build_plan(&self, statement: &str) -> Result<qfs_core::Plan, EngineError> {
        let stmt = qfs_exec::parse(statement).map_err(|e| map_exec_err(&e))?;
        qfs_exec::build_plan(&stmt, &self.engine).map_err(|e| map_exec_err(&e))
    }

    fn commit_policy(&self) -> qfs_mcp::Policy {
        // Default-deny is the law for this UNAUTHENTICATED, unattended surface: there is no
        // per-statement policy attached over MCP yet, so a write effect is refused with the policy
        // decision until a real policy is wired (a later ticket). The CLI's allow-all capability
        // shortcut is deliberately NOT reused here — an MCP tool call gets no privileged shortcut.
        qfs_mcp::default_deny_policy()
    }

    fn apply(&self, plan: &qfs_core::Plan) -> Result<(), EngineError> {
        // Offload to a dedicated OS thread so `apply_plan`'s `block_on` does not run on the serve
        // tokio runtime's reactor thread (which would panic). The commit drives the SAME runtime
        // interpreter + live driver registry the CLI `qfs run --commit` path uses.
        let result = std::thread::scope(|s| s.spawn(|| crate::commit::apply_plan(plan)).join());
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(map_exec_err(&e)),
            Err(_) => Err(EngineError::internal("the commit worker thread panicked")),
        }
    }

    fn safety_mode(&self) -> qfs_core::SafetyMode {
        // t59: resolve the active selectable safety mode LIVE per commit — the persisted
        // /sys/settings choice, else the env config, else the safe default. Resolving per call (not
        // once at boot) lets an operator change the mode (an `INSERT INTO /sys/settings`) take effect
        // without restarting serve; the read is cheap and a commit already touches the System DB.
        crate::sys::resolve_active_safety_mode()
    }

    fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError> {
        // Best-effort, redacted: open the SAME envelope-encrypted store `qfs account list`
        // reads, list selectors + metadata ONLY (never credential material). If the store cannot
        // be unlocked (no passphrase / no DB), report an empty list rather than failing — the MCP
        // surface never blocks on a locked credential store, and never leaks a secret.
        let store = match crate::connection::open_store_for_commit() {
            Some(store) => store,
            None => return Ok(Vec::new()),
        };
        let records = store
            .list(None)
            .map_err(|_| EngineError::new("list_failed", "could not list connections"))?;
        Ok(records
            .into_iter()
            .map(|r| ConnectionInfo {
                driver: r.driver.0,
                connection: r.connection.as_str().to_string(),
                created_at: format_rfc3339(r.created_at),
            })
            .collect())
    }
}

/// Format a stored connection's `created_at` as RFC 3339 (plaintext metadata, no secret). Falls
/// back to the empty string on the impossible-format case rather than panicking.
fn format_rfc3339(ts: time::OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Serve one `POST /mcp` request through the [`qfs_mcp::McpBinding`], adapting between the
/// listener's native [`qfs_http`] request/response DTOs and the pure [`qfs_http_core`] DTOs the
/// binding speaks. This is the seam the binary composes into the qfs-http listener's `Fallback`
/// (so qfs-mcp itself depends on neither qfs-http nor tokio).
#[must_use]
pub fn serve_mcp_request(
    binding: &qfs_mcp::McpBinding,
    req: &qfs_http::HttpRequest,
) -> qfs_http::HttpResponse {
    let core_req = to_core_request(req);
    let core_resp = binding.handle(&core_req);
    to_http_response(&core_resp)
}

/// Adapt the listener's [`qfs_http::HttpRequest`] onto the pure [`qfs_mcp::HttpRequest`] the
/// binding consumes (method + path-as-url + headers + body).
fn to_core_request(req: &qfs_http::HttpRequest) -> qfs_mcp::HttpRequest {
    let method = match req.method {
        qfs_http::Method::Post => qfs_mcp::HttpMethod::Post,
        qfs_http::Method::Put => qfs_mcp::HttpMethod::Put,
        qfs_http::Method::Patch => qfs_mcp::HttpMethod::Patch,
        qfs_http::Method::Delete => qfs_mcp::HttpMethod::Delete,
        // GET and any other token map to GET — the binding rejects every non-POST method anyway,
        // so the exact non-POST mapping does not matter (it only checks `== Post`).
        qfs_http::Method::Get | qfs_http::Method::Other(_) => qfs_mcp::HttpMethod::Get,
    };
    let mut core = qfs_mcp::HttpRequest::new(method, req.path.clone());
    for (k, v) in &req.headers {
        core = core.header(k.clone(), v.clone());
    }
    if !req.body.is_empty() {
        core = core.with_body(req.body.clone());
    }
    core
}

/// Adapt the binding's [`qfs_mcp::HttpResponse`] back onto the listener's
/// [`qfs_http::HttpResponse`], carrying the `content-type` as the response content type and every
/// OTHER header (notably the `401`'s `WWW-Authenticate` challenge, t50) through verbatim — so the
/// bearer-discovery hint actually reaches the client rather than being dropped at the adapter seam.
fn to_http_response(resp: &qfs_mcp::HttpResponse) -> qfs_http::HttpResponse {
    let content_type = resp
        .header_value("content-type")
        .unwrap_or("application/json")
        .to_string();
    let mut out = qfs_http::HttpResponse::new(resp.status, content_type, resp.body.clone());
    for (name, value) in &resp.headers {
        // `content-type` is carried as the dedicated field above; pass everything else through.
        if !name.eq_ignore_ascii_case("content-type") {
            out = out.with_header(name.clone(), value.clone());
        }
    }
    out
}

/// The bearer-validating [`McpAuthorizer`] (t50): the resource server's gate in FRONT of the MCP
/// tool surface. It extracts the `Authorization: Bearer <jwt>` access token, verifies it against the
/// active JWKS (signature + `iss`/`aud`/`exp`, via the pure [`qfs_oauth::verify_access_token`]), and
/// on success allows the request to reach a tool; on any failure it returns a `401` carrying a
/// `WWW-Authenticate: Bearer resource_metadata="<PRM url>"` challenge (RFC 9728) so a spec-compliant
/// client discovers the AS and re-authorizes.
///
/// ## Token hygiene (RFD §10)
/// The token is read from the `Authorization` header (already redaction-covered in
/// `qfs_http_core::SENSITIVE_HEADERS`) and is NEVER logged: the deny reason is a fixed, secret-free
/// sentence and the [`qfs_oauth::AccessTokenError`] variant names the failing condition only. Access
/// tokens are stateless (verified by signature, not a DB lookup) so there is no per-request store hit;
/// `exp` is the sole lifetime bound and is honored here.
pub struct BearerAuthorizer {
    /// The published JWKS the access-token signature is verified against.
    jwks: qfs_oauth::Jwks,
    /// The expected token issuer (the AS origin) — proxy-aware (matches what the client sees).
    issuer: String,
    /// The expected token audience (the MCP resource URL) — an audience-confusion guard.
    audience: String,
    /// The verbatim `WWW-Authenticate` challenge value returned on every reject (points at the PRM).
    challenge: String,
}

impl BearerAuthorizer {
    /// Build the authorizer from the AS verification material (the JWKS + the issuer/audience the
    /// token must bind, and the PRM URL the challenge points at).
    #[must_use]
    pub fn new(jwks: qfs_oauth::Jwks, issuer: String, audience: String, prm_url: &str) -> Self {
        Self {
            jwks,
            issuer,
            audience,
            // RFC 9728 §5.1 / the MCP auth spec: the challenge carries the protected-resource
            // metadata URL so the client can discover the authorization server and start the flow.
            challenge: format!("Bearer resource_metadata=\"{prm_url}\""),
        }
    }

    /// The current Unix time (seconds) used for the `exp` check. A clock failure yields `0` (which
    /// fails-closed: every non-expired token has `exp > 0`, so a `0` clock rejects everything).
    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Build the secret-free deny verdict carrying the discovery challenge.
    fn deny(&self, reason: &str) -> qfs_mcp::AuthDecision {
        qfs_mcp::AuthDecision::Deny {
            reason: reason.to_string(),
            challenge: Some(self.challenge.clone()),
        }
    }
}

impl qfs_mcp::McpAuthorizer for BearerAuthorizer {
    fn authorize(&self, req: &qfs_mcp::HttpRequest) -> qfs_mcp::AuthDecision {
        // Extract the bearer token from the `Authorization` header (case-insensitive scheme).
        let Some(header) = req.header_value("authorization") else {
            return self.deny("missing bearer token");
        };
        let Some(token) = strip_bearer(header) else {
            return self.deny("malformed Authorization header (expected `Bearer <token>`)");
        };
        // Verify signature + iss/aud/exp. Any failure is a 401 with the discovery challenge; the
        // specific AccessTokenError is intentionally NOT surfaced to the client (and never the token).
        match qfs_oauth::verify_access_token(
            token,
            &self.jwks,
            &self.issuer,
            &self.audience,
            Self::now_unix(),
        ) {
            Ok(_verified) => qfs_mcp::AuthDecision::Allow,
            Err(_e) => self.deny("invalid or expired access token"),
        }
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header value (the scheme is matched
/// case-insensitively per RFC 7235; surrounding whitespace is trimmed). Returns `None` for a missing
/// scheme or an empty token. `get(..7)` never panics on a non-ASCII boundary (it yields `None`).
fn strip_bearer(header: &str) -> Option<&str> {
    let prefix = header.get(..7)?;
    if !prefix.eq_ignore_ascii_case("Bearer ") {
        return None;
    }
    let token = header[7..].trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_mcp::{AuthDecision, HttpMethod, HttpRequest, McpAuthorizer, McpBinding, MCP_PATH};
    use qfs_oauth::{access_token_claims, sign_jws, Jwks, SigningKey};
    use serde_json::{json, Value};

    const ISS: &str = "http://localhost:8787";

    fn audience() -> String {
        format!("{ISS}{MCP_PATH}")
    }

    fn prm_url() -> String {
        format!("{ISS}/.well-known/oauth-protected-resource")
    }

    fn signing_key() -> SigningKey {
        SigningKey::generate(&[3u8; 32]).unwrap()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Mint a signed access token bound to `aud` with the given absolute `exp` (via iat + ttl).
    fn token(key: &SigningKey, aud: &str, iat: u64, ttl: u64) -> String {
        let claims = access_token_claims(ISS, aud, 7, "mcp:read", "client-1", iat, ttl);
        sign_jws(&claims, key).unwrap()
    }

    fn authorizer(key: &SigningKey) -> BearerAuthorizer {
        BearerAuthorizer::new(
            Jwks::new(vec![key.public_jwk()]),
            ISS.to_string(),
            audience(),
            &prm_url(),
        )
    }

    /// A no-op engine so the binding's tool dispatch can run once the authorizer allows the request.
    struct StubEngine;
    impl qfs_mcp::McpEngine for StubEngine {
        fn describe(&self, path: &str) -> Result<Value, qfs_mcp::EngineError> {
            Ok(json!({ "path": path }))
        }
        fn build_plan(&self, _s: &str) -> Result<qfs_core::Plan, qfs_mcp::EngineError> {
            Ok(qfs_core::Plan::pure())
        }
        fn commit_policy(&self) -> qfs_mcp::Policy {
            qfs_mcp::default_deny_policy()
        }
        fn apply(&self, _p: &qfs_core::Plan) -> Result<(), qfs_mcp::EngineError> {
            Ok(())
        }
        fn connections(&self) -> Result<Vec<qfs_mcp::ConnectionInfo>, qfs_mcp::EngineError> {
            Ok(vec![])
        }
    }

    fn post_with_auth(auth: Option<&str>) -> HttpRequest {
        let body = serde_json::to_vec(&json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        }))
        .unwrap();
        let mut req = HttpRequest::new(HttpMethod::Post, MCP_PATH)
            .header("content-type", "application/json")
            .with_body(body);
        if let Some(value) = auth {
            req = req.header("authorization", value);
        }
        req
    }

    #[test]
    fn no_token_is_denied_with_a_resource_metadata_challenge() {
        let key = signing_key();
        let decision = authorizer(&key).authorize(&post_with_auth(None));
        match decision {
            AuthDecision::Deny { challenge, .. } => {
                let c = challenge.expect("a WWW-Authenticate challenge");
                assert!(c.starts_with("Bearer "), "{c}");
                assert!(c.contains("resource_metadata="), "{c}");
                assert!(c.contains("oauth-protected-resource"), "{c}");
            }
            AuthDecision::Allow => panic!("a request with no token must be denied"),
        }
    }

    #[test]
    fn a_valid_token_is_allowed_and_the_tool_runs() {
        let key = signing_key();
        let tok = token(&key, &audience(), now(), 600);
        // The authorizer admits it.
        assert_eq!(
            authorizer(&key).authorize(&post_with_auth(Some(&format!("Bearer {tok}")))),
            AuthDecision::Allow
        );
        // And end to end through the binding, the tool dispatch runs (200 with a tools list).
        let binding = McpBinding::with_authorizer(Arc::new(StubEngine), Arc::new(authorizer(&key)));
        let resp = binding.handle(&post_with_auth(Some(&format!("Bearer {tok}"))));
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["result"]["tools"].is_array(), "the tools list ran: {v}");
    }

    #[test]
    fn an_expired_token_is_denied() {
        let key = signing_key();
        // iat far in the past, ttl 1s → long expired.
        let tok = token(&key, &audience(), now() - 10_000, 1);
        let binding = McpBinding::with_authorizer(Arc::new(StubEngine), Arc::new(authorizer(&key)));
        let resp = binding.handle(&post_with_auth(Some(&format!("Bearer {tok}"))));
        assert_eq!(resp.status, 401);
        assert!(resp.header_value("WWW-Authenticate").is_some());
    }

    #[test]
    fn a_wrong_audience_token_is_denied() {
        let key = signing_key();
        let tok = token(&key, "http://localhost:8787/not-mcp", now(), 600);
        assert!(matches!(
            authorizer(&key).authorize(&post_with_auth(Some(&format!("Bearer {tok}")))),
            AuthDecision::Deny { .. }
        ));
    }

    #[test]
    fn a_token_from_a_foreign_key_is_denied() {
        let key = signing_key();
        let foreign = SigningKey::generate(&[9u8; 32]).unwrap();
        // Signed by a key NOT in the authorizer's JWKS.
        let tok = token(&foreign, &audience(), now(), 600);
        assert!(matches!(
            authorizer(&key).authorize(&post_with_auth(Some(&format!("Bearer {tok}")))),
            AuthDecision::Deny { .. }
        ));
    }

    #[test]
    fn a_malformed_authorization_header_is_denied() {
        let key = signing_key();
        for bad in ["", "Bearer ", "Basic abc", "token-without-scheme"] {
            assert!(
                matches!(
                    authorizer(&key).authorize(&post_with_auth(Some(bad))),
                    AuthDecision::Deny { .. }
                ),
                "{bad:?} must be denied"
            );
        }
    }

    #[test]
    fn strip_bearer_is_case_insensitive_and_trims() {
        assert_eq!(strip_bearer("Bearer abc"), Some("abc"));
        assert_eq!(strip_bearer("bearer abc"), Some("abc"));
        assert_eq!(strip_bearer("BEARER  abc  "), Some("abc"));
        assert_eq!(strip_bearer("Bearer "), None);
        assert_eq!(strip_bearer("Basic abc"), None);
        assert_eq!(strip_bearer(""), None);
    }

    #[test]
    fn the_response_adapter_carries_www_authenticate_through() {
        // A 401 from the binding (no token) must keep its WWW-Authenticate header across the
        // qfs-http-core → qfs-http adapter (it is not the content-type, so it could be dropped).
        let key = signing_key();
        let binding = McpBinding::with_authorizer(Arc::new(StubEngine), Arc::new(authorizer(&key)));
        let core_resp = binding.handle(&post_with_auth(None));
        let http_resp = to_http_response(&core_resp);
        assert_eq!(http_resp.status, 401);
        assert!(
            http_resp
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("WWW-Authenticate")),
            "the challenge survived the adapter: {:?}",
            http_resp.headers
        );
    }
}
