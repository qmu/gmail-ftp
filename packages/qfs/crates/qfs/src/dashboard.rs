//! The embedded **SPA dashboard shell** (ticket t51): the *second of the three faces* of the one
//! qfs engine. A static single-page app — compiled INTO the `qfs` binary — that the in-house HTTP
//! listener serves over loopback, plus a thin JSON bridge that forwards a browser-composed qfs
//! statement into the SAME `describe → preview` engine path the CLI and the MCP face already use.
//!
//! ## One engine, three faces — no privileged shortcut
//! The bridge does NOT re-implement an executor. It drives the injected [`qfs_mcp::McpEngine`] — the
//! exact engine the t47 `POST /mcp` face is built on — so the dashboard, the CLI, and MCP all share
//! one statement-execution adapter. The constraint the roadmap names is enforced from day one: the
//! dashboard exposes **no capability the CLI/MCP lack**.
//!
//! ## This slice is the SHELL — preview/read ONLY
//! - `describe` is PURE (no creds, no I/O, no network) — exactly `qfs describe <path>`.
//! - `preview` builds the effect plan and renders its secret-free dry-run summary, applying ZERO
//!   effects (exactly the MCP `preview` tool).
//! - There is **no commit path here by design**. A `commit` mode is REFUSED — the irreversible
//!   approval cards are t52, and adding a shortcut commit here would break the one-engine rule.
//!
//! ## Secret discipline (RFD §10)
//! The bridge never returns credential material: describe is pure, the preview is a secret-free
//! plan summary, and engine errors are surfaced as the owned, secret-free [`qfs_mcp::EngineError`]
//! (`{ "error": { "code", "message" } }`) — never a raw upstream error, token, or path-secret. The
//! browser-supplied statement is parsed and planned through the normal pipeline (no string-splicing),
//! so a request value carries zero parse-time injection surface. No connection/credential listing is
//! served to the browser in this slice.
//!
//! ## Session gate seam (deferred to t46/t50, documented hook)
//! The shell is served loopback-only (the [`qfs_http::DEFAULT_BIND_ADDR`] default) and is NOT yet
//! gated on a session cookie: t46 opened the session store but no endpoint is gated on it yet, so —
//! per the ticket — this slice ships the loopback-only posture rather than inventing bespoke auth.
//! When identity is turned on, the `/` and `/api/*` routes are where a session check lands (a 401 +
//! sign-in redirect); that wiring is a follow-up, called out here rather than guessed at.
//!
//! ## Self-contained assets (offline-clean)
//! The HTML/CSS/JS are embedded via [`include_str!`] (mirroring `qfs-skill`) so they SHIP in the
//! binary and are never dead-stripped — no external CDN/font/script, so `qfs serve` stays
//! offline-clean and the hermetic-test rule holds.

use qfs_http::{HttpRequest, HttpResponse, Method};
use qfs_mcp::{EngineError, McpEngine};
use serde::Deserialize;

/// The embedded SPA assets (compiled into the binary, mirroring the `qfs-skill` `include_str!`
/// pattern so they ship in the artifact and are not dead-stripped).
const INDEX_HTML: &str = include_str!("../assets/dashboard/index.html");
/// The embedded stylesheet.
const APP_CSS: &str = include_str!("../assets/dashboard/app.css");
/// The embedded behaviour script.
const APP_JS: &str = include_str!("../assets/dashboard/app.js");

/// The root path the SPA shell is served at (`GET /`).
pub const DASHBOARD_ROOT: &str = "/";
/// The asset path prefix (`GET /assets/...`).
pub const ASSET_PREFIX: &str = "/assets/";
/// The thin bridge: a pure describe report for a posted path (`POST /api/describe`).
pub const API_DESCRIBE: &str = "/api/describe";
/// The thin bridge: a dry-run plan preview for a posted statement (`POST /api/run`).
pub const API_RUN: &str = "/api/run";
/// The reserved bridge prefix (every `/api/...` path is owned by the dashboard once mounted).
pub const API_PREFIX: &str = "/api/";

/// The describe-bridge request body (`{ "path": "/mail/drafts" }`).
#[derive(Debug, Deserialize)]
struct DescribeRequest {
    /// The absolute qfs path to introspect (pure describe; no creds, no I/O).
    path: String,
}

/// The run-bridge request body (`{ "statement": "...", "mode": "preview" }`). `mode` is optional and
/// defaults to preview; a `commit` mode is explicitly REFUSED in this shell (commit is t52).
#[derive(Debug, Deserialize)]
struct RunRequest {
    /// The browser-composed qfs statement (parsed + planned through the normal pipeline).
    statement: String,
    /// The requested mode. `None`/`"preview"`/`"read"` → the dry-run preview; anything else (notably
    /// `"commit"`) is refused — this shell has no apply path.
    #[serde(default)]
    mode: Option<String>,
}

/// Serve a dashboard route, if this request targets one. Returns `Some(response)` for a path the
/// shell OWNS (`GET /`, `GET /assets/*`, `POST /api/*`) and `None` otherwise — so the binary composes
/// this into the listener's [`qfs_http::Fallback`] chain ahead of the final 404, exactly like the
/// watchtower webhook ingest and the MCP `POST /mcp` handler.
///
/// The `engine` is the injected [`McpEngine`] the binary already built for the MCP face — reused
/// verbatim so the two faces share one engine path (no second executor).
#[must_use]
pub fn serve_dashboard(engine: &dyn McpEngine, req: &HttpRequest) -> Option<HttpResponse> {
    match req.method {
        // The shell page itself.
        Method::Get if req.path == DASHBOARD_ROOT => Some(index_response()),
        // A named static asset (or a 404 for an unknown asset under the prefix).
        Method::Get if req.path.starts_with(ASSET_PREFIX) => Some(asset_response(&req.path)),
        // The thin JSON bridge — preview/read only, through the SAME engine the CLI/MCP use.
        Method::Post if req.path == API_DESCRIBE => Some(describe_response(engine, &req.body)),
        Method::Post if req.path == API_RUN => Some(run_response(engine, &req.body)),
        // Any other method/path under the reserved bridge prefix → a legible JSON 404 (the shell
        // owns the whole `/api/` namespace so a typo does not silently fall through to the 404 page).
        _ if req.path.starts_with(API_PREFIX) => Some(json_error(
            404,
            &EngineError::new(
                "not_found",
                "no dashboard bridge route matches this method and path",
            ),
        )),
        // Not a dashboard path — let the rest of the fallback chain (then the 404) handle it.
        _ => None,
    }
}

/// The shell page (`GET /`). `no-cache` so a rebuilt binary's shell is picked up immediately (the
/// page is tiny; only the `/assets/*` bundle is cached).
fn index_response() -> HttpResponse {
    HttpResponse::new(
        200,
        "text/html; charset=utf-8",
        INDEX_HTML.as_bytes().to_vec(),
    )
    .with_header("Cache-Control", "no-cache")
}

/// A named static asset (`GET /assets/<name>`), or a JSON 404 for an unknown one. The assets are
/// content-stable per binary build, so they carry a modest immutable-ish cache header.
fn asset_response(path: &str) -> HttpResponse {
    let (body, content_type) = match path {
        "/assets/app.css" => (APP_CSS.as_bytes(), "text/css; charset=utf-8"),
        "/assets/app.js" => (APP_JS.as_bytes(), "application/javascript; charset=utf-8"),
        _ => {
            return json_error(
                404,
                &EngineError::new("not_found", "no such embedded dashboard asset"),
            )
        }
    };
    HttpResponse::new(200, content_type, body.to_vec())
        .with_header("Cache-Control", "public, max-age=3600")
}

/// The describe bridge (`POST /api/describe`): decode `{ path }`, return the cred-free describe
/// report verbatim (the SAME JSON `qfs describe <path>` and the MCP `describe` tool return). PURE.
fn describe_response(engine: &dyn McpEngine, body: &[u8]) -> HttpResponse {
    let req: DescribeRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e),
    };
    match engine.describe(&req.path) {
        Ok(report) => json_ok(&report),
        Err(e) => json_error(engine_status(&e), &e),
    }
}

/// The run bridge (`POST /api/run`): decode `{ statement, mode? }`, build the effect plan, and
/// return its secret-free dry-run preview — applying ZERO effects. A `commit` mode is REFUSED (this
/// shell has no apply path; commit is t52's gated card).
fn run_response(engine: &dyn McpEngine, body: &[u8]) -> HttpResponse {
    let req: RunRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e),
    };
    // Preview/read only. Any non-preview mode (notably `commit`) is refused HERE — no shortcut
    // apply path exists in the shell, so the one-engine safety floor cannot be bypassed.
    match req.mode.as_deref() {
        None | Some("preview") | Some("read") => {}
        Some(other) => {
            return json_error(
                422,
                &EngineError::new(
                    "unsupported_mode",
                    format!(
                        "the dashboard shell serves preview/read only; `{other}` is not available \
                         here (commit/apply is a later milestone)"
                    ),
                ),
            )
        }
    }
    let plan = match engine.build_plan(&req.statement) {
        Ok(p) => p,
        Err(e) => return json_error(engine_status(&e), &e),
    };
    // Zero effects: only the dry-run summary of the built plan (the exact MCP `preview` shape).
    let preview = qfs_exec::plan_preview(&plan);
    match serde_json::to_value(&preview) {
        Ok(v) => json_ok(&v),
        Err(e) => json_error(500, &EngineError::internal(e.to_string())),
    }
}

/// Map a secret-free engine error onto an HTTP status: an unknown mount is a 404, everything else a
/// 422 (the request's statement/path cannot be processed) — never a 500 for a caller-shaped error.
fn engine_status(e: &EngineError) -> u16 {
    match e.code.as_str() {
        "unknown_mount" => 404,
        "internal" => 500,
        _ => 422,
    }
}

/// Render a successful JSON payload (`200 application/json`).
fn json_ok(value: &serde_json::Value) -> HttpResponse {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"could not encode result"}}"#.to_vec()
    });
    HttpResponse::new(200, "application/json", body)
}

/// Render a secret-free engine error as a JSON problem body (`{ "error": { "code", "message" } }`),
/// mirroring the `crates/http` error mapping but in the MCP engine-error shape the bridge speaks.
fn json_error(status: u16, err: &EngineError) -> HttpResponse {
    let body = serde_json::json!({ "error": { "code": err.code, "message": err.message } });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"could not encode error"}}"#.to_vec()
    });
    HttpResponse::new(status, "application/json", bytes)
}

/// A malformed request body → a 400 with a generic, secret-free detail (the raw serde error text is
/// not echoed — it could quote attacker-supplied bytes; the class is enough for the caller to fix).
fn bad_request(_e: &serde_json::Error) -> HttpResponse {
    json_error(
        400,
        &EngineError::new(
            "bad_request",
            "request body must be a JSON object with the expected fields",
        ),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_mcp::ConnectionInfo;
    use serde_json::{json, Value};

    /// A stub engine: a fixed describe report + a pure (effect-free) plan, so the dashboard bridge
    /// can be exercised without the live driver registry. Mirrors the MCP tests' `StubEngine`.
    struct StubEngine;
    impl McpEngine for StubEngine {
        fn describe(&self, path: &str) -> Result<Value, EngineError> {
            if path == "/nope" {
                return Err(EngineError::new(
                    "unknown_mount",
                    "no driver is mounted for `/nope`",
                ));
            }
            Ok(json!({ "path": path, "archetype": "relational_table" }))
        }
        fn build_plan(&self, statement: &str) -> Result<qfs_core::Plan, EngineError> {
            if statement.contains("BOOM") {
                return Err(EngineError::new("parse", "unexpected token"));
            }
            Ok(qfs_core::Plan::pure())
        }
        fn commit_policy(&self) -> qfs_mcp::Policy {
            qfs_mcp::default_deny_policy()
        }
        fn apply(&self, _plan: &qfs_core::Plan) -> Result<(), EngineError> {
            panic!("the dashboard shell must NEVER reach apply (preview-only)");
        }
        fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError> {
            panic!("the dashboard shell must NEVER list connections to the browser");
        }
    }

    fn get(path: &str) -> HttpRequest {
        HttpRequest::new(Method::Get, path)
    }

    fn post(path: &str, body: Value) -> HttpRequest {
        let mut req = HttpRequest::new(Method::Post, path);
        req.body = serde_json::to_vec(&body).unwrap();
        req
    }

    #[test]
    fn root_serves_the_html_shell_with_the_right_content_type() {
        let resp = serve_dashboard(&StubEngine, &get("/")).expect("/ is owned");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "text/html; charset=utf-8");
        let html = resp.body_text();
        assert!(
            html.contains("<title>qfs dashboard</title>"),
            "the shell page: {html}"
        );
        // Self-contained: no external CDN/script reference leaks into the embedded shell.
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no external URL: {html}"
        );
    }

    #[test]
    fn assets_are_served_with_correct_content_types() {
        let css = serve_dashboard(&StubEngine, &get("/assets/app.css")).expect("css owned");
        assert_eq!(css.status, 200);
        assert_eq!(css.content_type, "text/css; charset=utf-8");
        assert!(
            css.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("cache-control")),
            "assets carry a Cache-Control header"
        );

        let js = serve_dashboard(&StubEngine, &get("/assets/app.js")).expect("js owned");
        assert_eq!(js.status, 200);
        assert_eq!(js.content_type, "application/javascript; charset=utf-8");
    }

    #[test]
    fn an_unknown_asset_404s() {
        let resp = serve_dashboard(&StubEngine, &get("/assets/missing.png")).expect("owned");
        assert_eq!(resp.status, 404);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
    }

    #[test]
    fn describe_bridge_returns_the_describe_json_shape() {
        let resp = serve_dashboard(
            &StubEngine,
            &post("/api/describe", json!({ "path": "/status" })),
        )
        .expect("owned");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "application/json");
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["path"], "/status");
        assert_eq!(v["archetype"], "relational_table");
    }

    #[test]
    fn describe_unknown_mount_is_a_404() {
        let resp = serve_dashboard(
            &StubEngine,
            &post("/api/describe", json!({ "path": "/nope" })),
        )
        .expect("owned");
        assert_eq!(resp.status, 404);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "unknown_mount");
    }

    #[test]
    fn run_bridge_returns_a_preview_json_shape() {
        let resp = serve_dashboard(
            &StubEngine,
            &post(
                "/api/run",
                json!({ "statement": "SELECT 1", "mode": "preview" }),
            ),
        )
        .expect("owned");
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        // The dry-run preview shape: a `preview` object and `committed: false` (NOTHING applied).
        assert!(v.get("preview").is_some(), "preview present: {v}");
        assert_eq!(v["committed"], json!(false));
    }

    #[test]
    fn run_bridge_defaults_to_preview_when_mode_is_absent() {
        let resp = serve_dashboard(
            &StubEngine,
            &post("/api/run", json!({ "statement": "SELECT 1" })),
        )
        .expect("owned");
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["committed"], json!(false));
    }

    #[test]
    fn run_bridge_refuses_a_commit_mode_with_no_apply_path() {
        // The one-engine safety floor: the shell has NO commit shortcut. A commit mode is refused
        // BEFORE the plan is even built — `StubEngine::apply` panics if ever reached.
        let resp = serve_dashboard(
            &StubEngine,
            &post(
                "/api/run",
                json!({ "statement": "REMOVE FROM /x", "mode": "commit" }),
            ),
        )
        .expect("owned");
        assert_eq!(resp.status, 422);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "unsupported_mode");
    }

    #[test]
    fn an_engine_error_is_a_secret_free_422() {
        let resp = serve_dashboard(
            &StubEngine,
            &post(
                "/api/run",
                json!({ "statement": "BOOM", "mode": "preview" }),
            ),
        )
        .expect("owned");
        assert_eq!(resp.status, 422);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "parse");
    }

    #[test]
    fn a_malformed_body_is_a_400_without_echoing_input() {
        let mut req = HttpRequest::new(Method::Post, "/api/run");
        req.body = b"not json at all {{".to_vec();
        let resp = serve_dashboard(&StubEngine, &req).expect("owned");
        assert_eq!(resp.status, 400);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "bad_request");
        // The raw (attacker-supplied) body bytes are NOT echoed back into the error detail.
        assert!(
            !resp.body_text().contains("not json at all"),
            "input not echoed"
        );
    }

    #[test]
    fn the_bridge_serves_no_connection_listing_to_the_browser() {
        // The shell owns the whole `/api/` namespace; a connections probe is a plain 404 (the
        // redacted-or-not connection list is NOT exposed to the browser in this slice). The stub's
        // `connections` panics if ever reached — proving the route never touches it.
        let resp =
            serve_dashboard(&StubEngine, &post("/api/connections", json!({}))).expect("owned");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn non_dashboard_paths_fall_through() {
        // `/mcp`, `/hooks/...`, and a declared endpoint are NOT the dashboard's — it returns None so
        // the rest of the fallback chain (then the 404) handles them.
        assert!(serve_dashboard(&StubEngine, &get("/mcp")).is_none());
        assert!(serve_dashboard(&StubEngine, &get("/hooks/x")).is_none());
        assert!(serve_dashboard(&StubEngine, &post("/mcp", json!({}))).is_none());
    }
}
