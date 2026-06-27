//! The [`McpBinding`] (t47): the leaf serve-binding that frames an HTTP `POST /mcp` request into
//! a JSON-RPC call, dispatches it through the [`crate::protocol`] core, and renders the response.
//!
//! ## Where the async lives (NOT here)
//! Mirroring the t34 watchtower webhook ingest, the MCP endpoint is served as a **pure,
//! synchronous handler** over the owned [`qfs_http_core`] DTOs. The `qfs` binary composes
//! [`McpBinding::handle`] into the existing in-house HTTP listener via the listener's request
//! `Fallback` seam (`POST /mcp` → this handler), so qfs-mcp needs NO tokio of its own and the
//! tokio I/O dead-ends in the terminal binary exactly as for the other serve leaves. The crate
//! stays a pure-ish leaf: it depends on qfs-server (the policy gate) + qfs-exec (the plan/preview)
//! but NEVER qfs-runtime (the apply is the injected [`crate::McpEngine::apply`] closure).
//!
//! ## Auth seam (t50) — explicit, untouched by tool logic
//! The [`McpAuthorizer`] trait is the EXPLICIT middleware seam where bearer/OAuth slots in FRONT of
//! the tool surface: the binding consults `authorize(req)` before any dispatch, and a [`AuthDecision::Deny`]
//! short-circuits to a `401` with NO tool ever invoked. As of t50 the `qfs` binary injects a real
//! bearer-verifying authorizer ([`McpBinding::with_authorizer`]) that validates the
//! `Authorization: Bearer <jwt>` access token (signature + `iss`/`aud`/`exp`) and, on failure,
//! returns a `WWW-Authenticate: Bearer …` challenge (RFC 9728 — pointing the client at the
//! protected-resource metadata so it can discover the AS and authorize). The default
//! [`AllowLocalhost`] authorizer (allow-all, localhost-only) is retained for the inert posture when
//! no AS is configured; it touches no tool handler either way. The authorizer NEVER logs the token
//! (the `Authorization` header is in `qfs_http_core::SENSITIVE_HEADERS`).

use std::sync::Arc;

use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};

use crate::jsonrpc::{ErrorObject, Request, Response, CODE_INVALID_REQUEST, CODE_PARSE_ERROR};
use crate::protocol::handle_request;
use crate::tools::McpEngine;

/// The path the MCP endpoint is mounted at (the binary routes the listener's `POST /mcp` here).
pub const MCP_PATH: &str = "/mcp";

/// The maximum MCP request body the binding accepts (a bounded buffer — RFD §6 resource
/// discipline, mirroring `qfs-http`'s bounded request). A larger body is refused with `413`
/// before any parse. The listener already caps the whole request at 1 MiB; this is the
/// MCP-payload-specific bound.
pub const MAX_MCP_BODY_BYTES: usize = 256 * 1024; // 256 KiB

/// The authentication decision for an inbound MCP request — the t50 seam's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    /// The request may proceed to tool dispatch.
    Allow,
    /// The request is refused (a `401`). Carries a secret-free `reason` (surfaced in the JSON-RPC
    /// error message) and an optional `challenge` — the verbatim `WWW-Authenticate` header value the
    /// binding attaches so a spec-compliant client re-discovers the AS (RFC 9728) and re-authorizes.
    /// The reason NEVER carries the token / a claim value (RFD §10).
    Deny {
        /// A secret-free reason (e.g. "missing bearer token", "invalid or expired token").
        reason: String,
        /// The `WWW-Authenticate` header value to return (e.g.
        /// `Bearer resource_metadata="https://…/.well-known/oauth-protected-resource"`), or `None`
        /// to omit the challenge.
        challenge: Option<String>,
    },
}

impl AuthDecision {
    /// A convenience constructor for a deny with no `WWW-Authenticate` challenge (e.g. a stub
    /// authorizer in tests).
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        AuthDecision::Deny {
            reason: reason.into(),
            challenge: None,
        }
    }
}

/// The middleware seam (t50) where authentication slots in FRONT of the tool surface. The binding
/// calls [`authorize`](McpAuthorizer::authorize) before dispatching; a [`AuthDecision::Deny`]
/// short-circuits with NO tool invoked. Implementations must be `Send + Sync` (shared across
/// connections) and must NEVER read or log secret material from the request.
pub trait McpAuthorizer: Send + Sync {
    /// Authorize (or refuse) an inbound MCP request.
    fn authorize(&self, req: &HttpRequest) -> AuthDecision;
}

/// The default authorizer for this milestone: permit every request. The endpoint is honestly
/// UNAUTHENTICATED and relies on the listener's localhost-only default bind (the name reflects the
/// deployment posture, not an enforced check). t50 replaces this with a real bearer/OAuth verifier.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowLocalhost;

impl McpAuthorizer for AllowLocalhost {
    fn authorize(&self, _req: &HttpRequest) -> AuthDecision {
        AuthDecision::Allow
    }
}

/// The MCP serving binding: holds the injected engine + the auth seam, and frames an HTTP request
/// into a JSON-RPC response. Constructed by the `qfs` binary's serve composition root.
pub struct McpBinding {
    engine: Arc<dyn McpEngine>,
    authorizer: Arc<dyn McpAuthorizer>,
}

impl McpBinding {
    /// Build a binding over an injected engine, with the default (allow-all, localhost-only)
    /// authorizer. t50 wires a real authorizer via [`McpBinding::with_authorizer`].
    #[must_use]
    pub fn new(engine: Arc<dyn McpEngine>) -> Self {
        Self {
            engine,
            authorizer: Arc::new(AllowLocalhost),
        }
    }

    /// Build a binding with an explicit authorizer (the t50 seam).
    #[must_use]
    pub fn with_authorizer(engine: Arc<dyn McpEngine>, authorizer: Arc<dyn McpAuthorizer>) -> Self {
        Self { engine, authorizer }
    }

    /// Frame `req` (a `POST /mcp` request) into the JSON-RPC response. The pipeline is:
    /// method/content-type validation → auth seam → bounded body → JSON-RPC parse → protocol
    /// dispatch → rendered [`HttpResponse`]. Total: every failure is a structured response, never
    /// a panic. A NOTIFICATION (no id) yields an empty `202 Accepted` (no JSON-RPC body to send).
    #[must_use]
    pub fn handle(&self, req: &HttpRequest) -> HttpResponse {
        // 1. Only POST is a valid MCP transport call.
        if req.method != HttpMethod::Post {
            return jsonrpc_error_response(
                405,
                ErrorObject::new(CODE_INVALID_REQUEST, "MCP requires POST"),
            );
        }
        // 2. Content-Type must be JSON (lenient prefix match — charset params allowed).
        let ct = req.header_value("content-type").unwrap_or("");
        if !ct.is_empty() && !ct.to_ascii_lowercase().contains("application/json") {
            return jsonrpc_error_response(
                415,
                ErrorObject::new(CODE_INVALID_REQUEST, "MCP requires application/json"),
            );
        }
        // 3. The auth seam (t50): refuse BEFORE any parse/dispatch. No tool runs on a deny. The
        //    `WWW-Authenticate` challenge (when supplied) points the client at the AS discovery
        //    document so it can re-authorize. The token is never logged (the reason is secret-free).
        if let AuthDecision::Deny { reason, challenge } = self.authorizer.authorize(req) {
            tracing::warn!(target: "qfs::mcp", "mcp request refused by authorizer");
            let mut resp = jsonrpc_error_response(
                401,
                ErrorObject::new(CODE_INVALID_REQUEST, format!("unauthorized: {reason}")),
            );
            if let Some(www_authenticate) = challenge {
                resp = resp.header("WWW-Authenticate", www_authenticate);
            }
            return resp;
        }
        // 4. Bounded body (RFD §6). An over-large payload is refused before parse.
        let body = req.body.as_deref().unwrap_or(&[]);
        if body.len() > MAX_MCP_BODY_BYTES {
            return jsonrpc_error_response(
                413,
                ErrorObject::new(CODE_INVALID_REQUEST, "MCP request body too large"),
            );
        }
        // 5. Parse the JSON-RPC request. A malformed body is a `-32700` parse error (HTTP 200 —
        //    the JSON-RPC error rides the response body, per the JSON-RPC convention).
        let rpc: Request = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(_) => {
                return jsonrpc_error_response(
                    200,
                    ErrorObject::new(CODE_PARSE_ERROR, "invalid JSON-RPC request"),
                );
            }
        };
        // 6. Dispatch through the pure protocol core.
        match handle_request(self.engine.as_ref(), &rpc) {
            Some(response) => json_response(200, &response),
            // A notification has no response object — 202 Accepted with an empty body.
            None => HttpResponse::new(202, Vec::new()).header("content-type", "application/json"),
        }
    }
}

/// Render a [`Response`] as a `200`/given-status JSON [`HttpResponse`]. A serialization failure
/// (not reachable for these owned DTOs) degrades to a minimal internal-error body, never a panic.
fn json_response(status: u16, response: &Response) -> HttpResponse {
    let body = serde_json::to_vec(response).unwrap_or_else(|_| {
        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"serialization failed"}}"#
            .to_vec()
    });
    HttpResponse::new(status, body).header("content-type", "application/json")
}

/// Render a transport-level JSON-RPC error (no id known) as an [`HttpResponse`] at `status`.
fn jsonrpc_error_response(status: u16, error: ErrorObject) -> HttpResponse {
    json_response(status, &Response::error(None, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ConnectionInfo, EngineError};
    use serde_json::{json, Value};

    struct StubEngine;
    impl McpEngine for StubEngine {
        fn describe(&self, path: &str) -> Result<Value, EngineError> {
            Ok(json!({ "path": path }))
        }
        fn build_plan(&self, _statement: &str) -> Result<qfs_core::Plan, EngineError> {
            Ok(qfs_core::Plan::pure())
        }
        fn commit_policy(&self) -> qfs_server::Policy {
            qfs_server::resolve_policy(None, &qfs_server::PolicyTable::new())
        }
        fn apply(&self, _plan: &qfs_core::Plan) -> Result<(), EngineError> {
            Ok(())
        }
        fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError> {
            Ok(vec![])
        }
    }

    fn post_json(body: Value) -> HttpRequest {
        HttpRequest::new(HttpMethod::Post, "/mcp")
            .header("content-type", "application/json")
            .with_body(serde_json::to_vec(&body).unwrap())
    }

    fn binding() -> McpBinding {
        McpBinding::new(Arc::new(StubEngine))
    }

    #[test]
    fn initialize_round_trips_over_http() {
        let resp = binding().handle(&post_json(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize","params":{}
        })));
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "qfs");
        assert_eq!(resp.header_value("content-type"), Some("application/json"));
    }

    #[test]
    fn non_post_is_refused() {
        let req = HttpRequest::new(HttpMethod::Get, "/mcp");
        let resp = binding().handle(&req);
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn wrong_content_type_is_refused() {
        let req = HttpRequest::new(HttpMethod::Post, "/mcp")
            .header("content-type", "text/plain")
            .with_body(b"{}".to_vec());
        let resp = binding().handle(&req);
        assert_eq!(resp.status, 415);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let req = HttpRequest::new(HttpMethod::Post, "/mcp")
            .header("content-type", "application/json")
            .with_body(b"not json".to_vec());
        let resp = binding().handle(&req);
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], CODE_PARSE_ERROR);
    }

    #[test]
    fn oversize_body_is_refused() {
        let big = vec![b'a'; MAX_MCP_BODY_BYTES + 1];
        let req = HttpRequest::new(HttpMethod::Post, "/mcp")
            .header("content-type", "application/json")
            .with_body(big);
        let resp = binding().handle(&req);
        assert_eq!(resp.status, 413);
    }

    #[test]
    fn notification_yields_202_no_body() {
        let resp = binding().handle(&post_json(json!({
            "jsonrpc":"2.0","method":"notifications/initialized"
        })));
        assert_eq!(resp.status, 202);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn authorizer_deny_short_circuits_before_dispatch() {
        struct DenyAll;
        impl McpAuthorizer for DenyAll {
            fn authorize(&self, _req: &HttpRequest) -> AuthDecision {
                AuthDecision::deny("no token")
            }
        }
        let b = McpBinding::with_authorizer(Arc::new(StubEngine), Arc::new(DenyAll));
        let resp = b.handle(&post_json(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        })));
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn a_deny_with_a_challenge_attaches_www_authenticate() {
        struct DenyWithChallenge;
        impl McpAuthorizer for DenyWithChallenge {
            fn authorize(&self, _req: &HttpRequest) -> AuthDecision {
                AuthDecision::Deny {
                    reason: "missing bearer token".to_string(),
                    challenge: Some(
                        "Bearer resource_metadata=\"https://qfs.example/.well-known/oauth-protected-resource\""
                            .to_string(),
                    ),
                }
            }
        }
        let b = McpBinding::with_authorizer(Arc::new(StubEngine), Arc::new(DenyWithChallenge));
        let resp = b.handle(&post_json(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        })));
        assert_eq!(resp.status, 401);
        let www = resp
            .header_value("WWW-Authenticate")
            .expect("challenge header");
        assert!(www.starts_with("Bearer "));
        assert!(
            www.contains("resource_metadata="),
            "the challenge points at the protected-resource metadata: {www}"
        );
    }
}
