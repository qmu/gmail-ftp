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
//! ## describe + preview + the t52 approval-card commit
//! - `describe` is PURE (no creds, no I/O, no network) — exactly `qfs describe <path>`.
//! - `preview` builds the effect plan and renders its secret-free dry-run summary, applying ZERO
//!   effects (exactly the MCP `preview` tool). `POST /api/run` stays preview/read only; a `commit`
//!   *mode* there is still refused (the apply path is the dedicated, gated `/api/commit`).
//! - `commit` (t52) routes a previewed statement through the SAME single commit path the MCP
//!   `commit` tool uses ([`qfs_mcp::commit_plan`]): the default-deny policy gate
//!   (`qfs_server::gate_plan`) THEN the [`qfs_core::IrreversibleGuard`]. A reversible in-policy
//!   plan auto-applies (roadmap §2.4 *Autonomous-in-policy*); an out-of-policy plan is REFUSED with
//!   the decision; an irreversible plan (REMOVE / CALL) WITHOUT an explicit ack is REFUSED with a
//!   legible "needs human approval" signal — the dashboard's one-time approval card supplies that
//!   ack as `ack=true` on a second, explicit confirm. The card gets NO capability the CLI/MCP lack.
//!
//! ## Secret discipline (RFD §10)
//! The bridge never returns credential material: describe is pure, the preview is a secret-free
//! plan summary, the commit outcome carries only the secret-free dry-run summary / per-effect
//! `"<VERB> <driver>:<path>"` labels, and engine errors are surfaced as the owned, secret-free
//! [`qfs_mcp::EngineError`] (`{ "error": { "code", "message" } }`) — never a raw upstream error,
//! token, or path-secret. The browser-supplied statement is parsed and planned through the normal
//! pipeline (no string-splicing), so a request value carries zero parse-time injection surface. No
//! connection/credential listing is served to the browser.
//!
//! ## Network posture — loopback-only; bearer-gating the commit is a documented follow-up
//! The shell is served loopback-only (the [`qfs_http::DEFAULT_BIND_ADDR`] default) and is NOT yet
//! gated on a session cookie or the t50 bearer token: t46/t50 opened the session/OAuth machinery
//! but no dashboard endpoint consumes a browser session→bearer mapping yet. Rather than invent a
//! bespoke auth surface, `/api/commit` keeps the loopback-only posture AND — crucially — inherits
//! the SAME default-deny policy gate the unauthenticated MCP surface relied on before its own
//! bearer gate: a dashboard commit can do nothing the engine's policy does not already grant, so it
//! opens no privileged, looser path to the network. Threading the t50 authorizer (and a single-use
//! ack token to defeat replay) through `/api/commit` is the called-out follow-up — flagged here
//! rather than half-wired.
//!
//! ## Self-contained assets (offline-clean)
//! The HTML/CSS/JS are embedded via [`include_str!`] (mirroring `qfs-skill`) so they SHIP in the
//! binary and are never dead-stripped — no external CDN/font/script, so `qfs serve` stays
//! offline-clean and the hermetic-test rule holds.

use qfs_http::{HttpRequest, HttpResponse, Method};
use qfs_mcp::{commit_plan, CommitOutcome, EngineError, McpEngine};
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
/// The gated commit bridge: apply a previewed statement through the one commit path (`POST
/// /api/commit`). The ONLY mutating dashboard endpoint (t52's approval-card target).
pub const API_COMMIT: &str = "/api/commit";
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
    /// `"commit"`) is refused — the apply path is the dedicated, gated `/api/commit`, not `/api/run`.
    #[serde(default)]
    mode: Option<String>,
}

/// The commit-bridge request body (`{ "statement": "...", "ack": false }`). `ack` is the approval
/// card's explicit confirmation: it is the SAME acknowledgement the CLI's `--commit-irreversible`
/// flag and the MCP `commit` tool's `ack=true` supply, flowing through the SAME
/// [`qfs_core::IrreversibleGuard`]. It defaults to `false`, so an irreversible plan is refused
/// (held for the card) unless the human explicitly confirms.
#[derive(Debug, Deserialize)]
struct CommitRequest {
    /// The browser-composed qfs statement to apply (the one the human previewed).
    statement: String,
    /// The explicit irreversible-effect acknowledgement (the approval card's second confirm).
    #[serde(default)]
    ack: bool,
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
        // The gated commit bridge (t52): apply through the one commit path (gate + guard).
        Method::Post if req.path == API_COMMIT => Some(commit_response(engine, &req.body)),
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

/// The commit bridge (`POST /api/commit`): decode `{ statement, ack? }` and route it through the
/// SAME single commit path the MCP `commit` tool uses ([`qfs_mcp::commit_plan`]) — the default-deny
/// policy gate THEN the [`qfs_core::IrreversibleGuard`], THEN the injected runtime-backed apply. The
/// dashboard adds NO commit logic of its own; it only renders the structured [`CommitOutcome`] as
/// secret-free JSON for the approval card:
///   - **applied** (reversible in-policy, or irreversible WITH `ack`): `200`,
///     `{ "applied": true, "committed": true, "preview": { … } }`.
///   - **policy refusal** (out of policy): `403`,
///     `{ "applied": false, "refused": "policy_denied", "reason", "effects": [ … ] }`.
///   - **needs approval** (irreversible, no `ack`): `200`,
///     `{ "applied": false, "refused": "needs_human_approval", "needs_ack": true, "reason",
///       "effects": [ … ] }` — the card shows the effects and asks for the explicit second confirm.
///   - **engine error** (parse / apply failure): the secret-free `{ "error": { code, message } }`.
fn commit_response(engine: &dyn McpEngine, body: &[u8]) -> HttpResponse {
    let req: CommitRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return bad_request(&e),
    };
    // The ONE commit path (no dashboard-local applier, no second gate/guard): a reversible in-policy
    // plan auto-applies, an out-of-policy plan is refused with the decision, and an irreversible plan
    // is held for the card's explicit ack — exactly what the CLI/MCP do for the same plan.
    match commit_plan(engine, &req.statement, req.ack) {
        CommitOutcome::Applied(committed) => match serde_json::to_value(&committed) {
            // `committed` serializes to `{ "preview": …, "committed": true }`; the browser also gets
            // a top-level `applied: true` so the card can branch without inspecting the summary.
            Ok(serde_json::Value::Object(mut map)) => {
                map.insert("applied".to_string(), serde_json::Value::Bool(true));
                json_ok(&serde_json::Value::Object(map))
            }
            Ok(other) => json_ok(&other),
            Err(e) => json_error(500, &EngineError::internal(e.to_string())),
        },
        CommitOutcome::PolicyDenied { reason, effects } => {
            json_refused(403, "policy_denied", &reason, &effects, false)
        }
        CommitOutcome::NeedsApproval { reason, effects } => {
            json_refused(200, "needs_human_approval", &reason, &effects, true)
        }
        CommitOutcome::Failed(e) => json_error(engine_status(&e), &e),
    }
}

/// Render a REFUSED commit (policy deny or needs-approval) as secret-free JSON: the stable refusal
/// tag + reason + the per-effect summaries, plus `applied: false` and a `needs_ack` flag the
/// approval card reads to decide whether to offer the explicit second confirm.
fn json_refused(
    status: u16,
    refused: &str,
    reason: &str,
    effects: &[String],
    needs_ack: bool,
) -> HttpResponse {
    let body = serde_json::json!({
        "applied": false,
        "refused": refused,
        "needs_ack": needs_ack,
        "reason": reason,
        "effects": effects,
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal","message":"could not encode refusal"}}"#.to_vec()
    });
    HttpResponse::new(status, "application/json", bytes)
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
                json!({ "statement": "REMOVE /x", "mode": "commit" }),
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

    // ---- t52: the gated `/api/commit` approval-card bridge -------------------------------------

    use std::sync::Mutex;

    /// A commit-capable stub: `build_plan` returns a configured effect plan, `commit_policy` returns
    /// a configured policy (default-deny if unset), and `apply` records that it was reached (the
    /// load-bearing "applied vs. not" assertion). Mirrors the MCP protocol tests' `FakeEngine` so the
    /// dashboard exercises the SAME gate + guard the MCP face does.
    #[derive(Default)]
    struct CommitStub {
        /// The plan `build_plan` yields (an effect plan); `None` ⇒ a pure (no-effect) plan.
        plan: Option<qfs_core::Plan>,
        /// The policy `commit` gates against; `None` ⇒ default-deny.
        policy: Option<qfs_mcp::Policy>,
        /// Set to `true` by `apply` — proves whether the injected applier was reached.
        applied: Mutex<bool>,
    }

    fn insert_plan() -> qfs_core::Plan {
        use qfs_core::{DriverId, EffectKind, EffectNode, NodeId, Plan, Target, VfsPath};
        let mut plan = Plan::pure();
        plan.nodes = vec![EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/drafts")),
        )];
        plan
    }

    fn remove_plan() -> qfs_core::Plan {
        use qfs_core::{DriverId, EffectKind, EffectNode, NodeId, Plan, Target, VfsPath};
        let mut plan = Plan::pure();
        plan.nodes = vec![EffectNode::new(
            NodeId(0),
            EffectKind::Remove,
            Target::new(DriverId::new("local"), VfsPath::new("/local/x")),
        )];
        plan
    }

    /// A policy that explicitly ALLOWs the given verbs on any driver (an explicit verb list, so an
    /// irreversible verb like REMOVE is genuinely granted by the gate).
    fn allow_policy(verbs: &[qfs_mcp::Verb]) -> qfs_mcp::Policy {
        use qfs_mcp::{DriverGlob, Policy, Rule, VerbSet};
        Policy::new("test").with_rule(Rule::allow(VerbSet::from_verbs(verbs), DriverGlob::any()))
    }

    impl McpEngine for CommitStub {
        fn describe(&self, path: &str) -> Result<Value, EngineError> {
            Ok(json!({ "path": path }))
        }
        fn build_plan(&self, statement: &str) -> Result<qfs_core::Plan, EngineError> {
            if statement.contains("BOOM") {
                return Err(EngineError::new("parse", "unexpected token"));
            }
            Ok(self.plan.clone().unwrap_or_else(qfs_core::Plan::pure))
        }
        fn commit_policy(&self) -> qfs_mcp::Policy {
            self.policy
                .clone()
                .unwrap_or_else(qfs_mcp::default_deny_policy)
        }
        fn apply(&self, _plan: &qfs_core::Plan) -> Result<(), EngineError> {
            *self.applied.lock().unwrap() = true;
            Ok(())
        }
        fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError> {
            panic!("the dashboard commit bridge must NEVER list connections to the browser");
        }
    }

    fn commit_resp(engine: &CommitStub, body: Value) -> (HttpResponse, Value) {
        let resp = serve_dashboard(engine, &post("/api/commit", body)).expect("/api/commit owned");
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        (resp, v)
    }

    #[test]
    fn commit_applies_a_reversible_in_policy_plan() {
        let engine = CommitStub {
            plan: Some(insert_plan()),
            policy: Some(allow_policy(&[qfs_mcp::Verb::Insert])),
            ..Default::default()
        };
        let (resp, v) = commit_resp(&engine, json!({ "statement": "INSERT INTO /mail/drafts" }));
        assert_eq!(resp.status, 200);
        assert_eq!(v["applied"], json!(true));
        assert_eq!(v["committed"], json!(true));
        assert!(
            v.get("preview").is_some(),
            "carries the committed summary: {v}"
        );
        assert!(*engine.applied.lock().unwrap(), "the applier was reached");
    }

    #[test]
    fn commit_refuses_an_out_of_policy_plan_with_the_decision() {
        // Default-deny policy ⇒ an INSERT is refused; the apply must NOT be reached (zero effects).
        let engine = CommitStub {
            plan: Some(insert_plan()),
            ..Default::default()
        };
        let (resp, v) = commit_resp(&engine, json!({ "statement": "INSERT INTO /mail/drafts" }));
        assert_eq!(resp.status, 403);
        assert_eq!(v["applied"], json!(false));
        assert_eq!(v["refused"], "policy_denied");
        assert!(
            !v["reason"].as_str().unwrap().is_empty(),
            "carries the decision: {v}"
        );
        assert!(
            !*engine.applied.lock().unwrap(),
            "an out-of-policy plan applies nothing"
        );
    }

    #[test]
    fn commit_refuses_an_irreversible_plan_without_ack() {
        // In-policy REMOVE, but no ack ⇒ held by the IrreversibleGuard (needs the card's confirm).
        let engine = CommitStub {
            plan: Some(remove_plan()),
            policy: Some(allow_policy(&[qfs_mcp::Verb::Remove])),
            ..Default::default()
        };
        let (resp, v) = commit_resp(&engine, json!({ "statement": "REMOVE /local/x" }));
        assert_eq!(resp.status, 200);
        assert_eq!(v["applied"], json!(false));
        assert_eq!(v["refused"], "needs_human_approval");
        assert_eq!(v["needs_ack"], json!(true));
        assert!(
            !*engine.applied.lock().unwrap(),
            "an unacked irreversible plan applies nothing"
        );
    }

    #[test]
    fn commit_applies_an_irreversible_plan_with_ack() {
        // The approval card's explicit second confirm (`ack=true`) flows through the SAME guard the
        // CLI's `--commit-irreversible` drives — so the acked irreversible plan applies.
        let engine = CommitStub {
            plan: Some(remove_plan()),
            policy: Some(allow_policy(&[qfs_mcp::Verb::Remove])),
            ..Default::default()
        };
        let (resp, v) = commit_resp(
            &engine,
            json!({ "statement": "REMOVE /local/x", "ack": true }),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(v["applied"], json!(true));
        assert_eq!(v["committed"], json!(true));
        assert!(
            *engine.applied.lock().unwrap(),
            "the acked apply was reached"
        );
    }

    #[test]
    fn commit_response_is_secret_free() {
        // The card payload carries only the dry-run summary + per-effect `<VERB> <driver>:<path>`
        // labels — never a row payload, credential, or token.
        let engine = CommitStub {
            plan: Some(remove_plan()),
            policy: Some(allow_policy(&[qfs_mcp::Verb::Remove])),
            ..Default::default()
        };
        let (_resp, v) = commit_resp(&engine, json!({ "statement": "REMOVE /local/x" }));
        let effects = v["effects"].as_array().expect("effects array");
        assert!(
            effects
                .iter()
                .all(|e| e.as_str().unwrap() == "REMOVE local:/local/x"),
            "effects are driver+path labels only: {v}"
        );
    }

    #[test]
    fn commit_propagates_an_engine_error_secret_free() {
        let engine = CommitStub::default();
        let (resp, v) = commit_resp(&engine, json!({ "statement": "BOOM" }));
        assert_eq!(resp.status, 422);
        assert_eq!(v["error"]["code"], "parse");
    }

    #[test]
    fn commit_malformed_body_is_a_400() {
        let mut req = HttpRequest::new(Method::Post, "/api/commit");
        req.body = b"not json at all {{".to_vec();
        let resp = serve_dashboard(&CommitStub::default(), &req).expect("owned");
        assert_eq!(resp.status, 400);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["error"]["code"], "bad_request");
        assert!(
            !resp.body_text().contains("not json at all"),
            "input not echoed"
        );
    }
}
