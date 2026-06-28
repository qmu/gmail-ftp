//! The MCP protocol core (t47): `initialize` / `tools/list` / `tools/call` (+ `ping`) over the
//! JSON-RPC framing, dispatched to the four tool handlers.
//!
//! PURE: [`handle_request`] is a function of `(engine, request)` with no I/O of its own (the
//! reads/plans/applies are the injected [`McpEngine`]). The wire shapes are golden-pinned by the
//! crate's tests, so a drift in `initialize` / `tools/list` / `tools/call` fails the build.
//!
//! ## Transport / spec version (a flagged product decision)
//! This implements the **plain HTTP request/response** transport — one JSON-RPC request object per
//! `POST /mcp`, one response object back (the simplest of the MCP HTTP transports; no SSE stream).
//! The advertised [`PROTOCOL_VERSION`] is `2025-06-18`. If the negotiated spec revs (streamable
//! HTTP, session ids), this is the seam that changes — the tool surface itself is unaffected.

use serde_json::{json, Value};

use crate::jsonrpc::{ErrorObject, Request, Response};
use crate::tools::{call_tool, tool_descriptors, McpEngine};

/// The MCP protocol version this server advertises in `initialize`. Date-versioned per the spec;
/// flagged in the module doc as a product decision (see the transport note).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The server identity advertised in `initialize.serverInfo.name`.
pub const SERVER_NAME: &str = "qfs";

/// Handle one JSON-RPC [`Request`] against the injected engine, returning the [`Response`] to
/// serialize — or `None` for a NOTIFICATION (no response is sent, per JSON-RPC §4.1).
///
/// The dispatch is total: an unknown method yields a `-32601` error response; a malformed
/// `tools/call` yields a `-32602` error response; a tool's own (engine) failure is reported
/// in-band as an `isError` tool result (a successful JSON-RPC response carrying the failure).
#[must_use]
pub fn handle_request(engine: &dyn McpEngine, req: &Request) -> Option<Response> {
    // Notifications are handled for effect only — never answered (e.g. `notifications/initialized`).
    if req.is_notification() {
        tracing::debug!(target: "qfs::mcp", method = %req.method, "mcp notification (no response)");
        return None;
    }
    let id = req.id.clone();
    let response = match req.method.as_str() {
        "initialize" => Response::result(id, initialize_result()),
        "tools/list" => Response::result(id, tools_list_result()),
        "tools/call" => match dispatch_tools_call(engine, req.params.as_ref()) {
            Ok(result) => Response::result(id, result),
            Err(err) => Response::error(id, err),
        },
        // MCP utility ping (liveness): an empty result object.
        "ping" => Response::result(id, json!({})),
        other => Response::error(id, ErrorObject::method_not_found(other)),
    };
    Some(response)
}

/// The `initialize` result: the advertised protocol version, the (tools-only) capabilities, and
/// the server identity. No credential, no live state — a static handshake.
#[must_use]
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// The `tools/list` result: the four tool descriptors (the frozen operating-loop surface).
#[must_use]
pub fn tools_list_result() -> Value {
    json!({ "tools": tool_descriptors() })
}

/// Extract `(name, arguments)` from the `tools/call` params and dispatch to [`call_tool`].
/// A missing `name` is a `-32602` protocol fault; absent `arguments` defaults to an empty object.
fn dispatch_tools_call(
    engine: &dyn McpEngine,
    params: Option<&Value>,
) -> Result<Value, ErrorObject> {
    let params = params.ok_or_else(|| {
        ErrorObject::invalid_params("tools/call requires params { name, arguments }")
    })?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ErrorObject::invalid_params("tools/call requires a string `name`"))?;
    // `arguments` is optional in the MCP schema; default to an empty object so a no-arg tool
    // (connections) needs no arguments key.
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);
    call_tool(engine, name, arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Request, CODE_INVALID_PARAMS, CODE_METHOD_NOT_FOUND};
    use crate::tools::{ConnectionInfo, EngineError, McpEngine};
    use std::sync::Mutex;

    /// A controllable fake engine for the wire-shape + safety tests. Each tool's behaviour is set
    /// per-test; `apply` records whether it was reached (the load-bearing "zero effects" assertion).
    #[derive(Default)]
    struct FakeEngine {
        /// The plan `build_plan` returns (an effect plan or a pure plan). `None` ⇒ a parse error.
        plan: Option<qfs_core::Plan>,
        /// The policy `commit` gates against (default = default-deny).
        policy: Option<qfs_server::Policy>,
        /// Set to `true` by `apply` — proves whether the applier was reached.
        applied: Mutex<bool>,
        /// Whether `apply` should report a failure.
        apply_fails: bool,
        /// The active t59 safety mode this engine reports (default = Autonomous-in-policy).
        mode: qfs_core::SafetyMode,
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

    /// A policy that explicitly ALLOWs the given verbs on any driver — an explicit verb list (not
    /// a broad `ALL`), so an irreversible verb (REMOVE/CALL) is genuinely granted by the gate (a
    /// bare `ALLOW ALL` is held back from the irreversible classes by the enforcer).
    fn allow_policy(verbs: &[qfs_server::Verb]) -> qfs_server::Policy {
        use qfs_server::{DriverGlob, Policy, Rule, VerbSet};
        Policy::new("test").with_rule(Rule::allow(VerbSet::from_verbs(verbs), DriverGlob::any()))
    }

    impl McpEngine for FakeEngine {
        fn describe(&self, path: &str) -> Result<Value, EngineError> {
            if path == "/unknown" {
                return Err(EngineError::new("unknown_mount", "no driver mounted"));
            }
            Ok(json!({ "path": path, "archetype": "AppendLog" }))
        }
        fn build_plan(&self, statement: &str) -> Result<qfs_core::Plan, EngineError> {
            if statement.contains("bad") {
                return Err(EngineError::new("parse", "syntax error"));
            }
            Ok(self.plan.clone().unwrap_or_else(qfs_core::Plan::pure))
        }
        fn commit_policy(&self) -> qfs_server::Policy {
            self.policy.clone().unwrap_or_else(|| {
                qfs_server::resolve_policy(None, &qfs_server::PolicyTable::new())
            })
        }
        fn apply(&self, _plan: &qfs_core::Plan) -> Result<(), EngineError> {
            *self.applied.lock().unwrap() = true;
            if self.apply_fails {
                return Err(EngineError::new("commit_failed", "a leg failed"));
            }
            Ok(())
        }
        fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError> {
            Ok(vec![ConnectionInfo {
                driver: "github".to_string(),
                connection: "work".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            }])
        }
        fn safety_mode(&self) -> qfs_core::SafetyMode {
            self.mode
        }
    }

    fn req(id: i64, method: &str, params: Value) -> Request {
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))
        .unwrap()
    }

    #[test]
    fn initialize_advertises_version_and_tools_capability() {
        let engine = FakeEngine::default();
        let resp = handle_request(&engine, &req(1, "initialize", json!({}))).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "qfs");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_returns_the_four_tools_in_order() {
        let engine = FakeEngine::default();
        let resp = handle_request(&engine, &req(1, "tools/list", json!({}))).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["describe", "preview", "commit", "connections"]);
        // Each descriptor carries a prescriptive description + a JSON-Schema input.
        for t in tools {
            assert!(t["description"].as_str().unwrap().len() > 20);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn unknown_method_is_a_method_not_found_error() {
        let engine = FakeEngine::default();
        let resp = handle_request(&engine, &req(1, "frobnicate", json!({}))).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], CODE_METHOD_NOT_FOUND);
    }

    #[test]
    fn notification_gets_no_response() {
        let engine = FakeEngine::default();
        let note: Request =
            serde_json::from_value(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .unwrap();
        assert!(handle_request(&engine, &note).is_none());
    }

    #[test]
    fn describe_tool_returns_pure_report_text() {
        let engine = FakeEngine::default();
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"describe","arguments":{"path":"/mail/drafts"}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("/mail/drafts"));
    }

    #[test]
    fn tools_call_missing_name_is_invalid_params() {
        let engine = FakeEngine::default();
        let resp = handle_request(&engine, &req(1, "tools/call", json!({"arguments":{}}))).unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], CODE_INVALID_PARAMS);
    }

    #[test]
    fn preview_applies_zero_effects() {
        let engine = FakeEngine {
            plan: Some(insert_plan()),
            ..Default::default()
        };
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"preview","arguments":{"statement":"INSERT ..."}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        // The applier was NEVER reached by a preview.
        assert!(
            !*engine.applied.lock().unwrap(),
            "preview must apply nothing"
        );
    }

    #[test]
    fn commit_out_of_policy_is_refused_without_applying() {
        // Default-deny policy ⇒ an INSERT is denied; the apply must not be reached.
        let engine = FakeEngine {
            plan: Some(insert_plan()),
            ..Default::default()
        };
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"commit","arguments":{"statement":"INSERT ..."}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("policy_denied"),
            "names the policy refusal: {text}"
        );
        assert!(
            !*engine.applied.lock().unwrap(),
            "a denied plan applies nothing"
        );
    }

    #[test]
    fn commit_in_policy_reversible_applies() {
        let engine = FakeEngine {
            plan: Some(insert_plan()),
            policy: Some(allow_policy(&[qfs_server::Verb::Insert])),
            ..Default::default()
        };
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"commit","arguments":{"statement":"INSERT ..."}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            v["result"]["isError"], false,
            "in-policy reversible commit applies"
        );
        assert!(*engine.applied.lock().unwrap(), "the applier was reached");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"committed\": true"));
    }

    #[test]
    fn commit_irreversible_without_ack_is_refused_not_applied() {
        // In-policy REMOVE, but no ack ⇒ blocked by the IrreversibleGuard (Server mode).
        let engine = FakeEngine {
            plan: Some(remove_plan()),
            policy: Some(allow_policy(&[qfs_server::Verb::Remove])),
            ..Default::default()
        };
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"commit","arguments":{"statement":"REMOVE ..."}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("needs_human_approval"),
            "legible approval signal: {text}"
        );
        assert!(
            !*engine.applied.lock().unwrap(),
            "an irreversible plan without ack applies nothing"
        );
    }

    #[test]
    fn commit_irreversible_with_ack_applies() {
        let engine = FakeEngine {
            plan: Some(remove_plan()),
            policy: Some(allow_policy(&[qfs_server::Verb::Remove])),
            ..Default::default()
        };
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"commit","arguments":{"statement":"REMOVE ...","ack":true}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        assert!(
            *engine.applied.lock().unwrap(),
            "an acked irreversible plan applies"
        );
    }

    #[test]
    fn connections_returns_redacted_listing() {
        let engine = FakeEngine::default();
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"connections","arguments":{}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("github") && text.contains("work"));
        // No secret material ever appears in the listing shape.
        assert!(!text.to_lowercase().contains("token") && !text.to_lowercase().contains("secret"));
    }

    #[test]
    fn engine_error_is_reported_in_band_as_is_error() {
        let engine = FakeEngine::default();
        let resp = handle_request(
            &engine,
            &req(
                1,
                "tools/call",
                json!({"name":"preview","arguments":{"statement":"bad"}}),
            ),
        )
        .unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        // A build failure is an in-band isError tool result (a successful JSON-RPC response).
        assert!(v.get("error").is_none());
        assert_eq!(v["result"]["isError"], true);
    }

    // --- t59: the three selectable safety presets, LIVE on the real `commit_plan` path -----------

    use crate::tools::{commit_plan, CommitOutcome};
    use qfs_core::SafetyMode;

    /// Build a FakeEngine for an in-policy commit under `mode` (the verb is explicitly allowed so
    /// the policy gate passes and the mode decision is what's exercised).
    fn engine_for(plan: qfs_core::Plan, verb: qfs_server::Verb, mode: SafetyMode) -> FakeEngine {
        FakeEngine {
            plan: Some(plan),
            policy: Some(allow_policy(&[verb])),
            mode,
            ..Default::default()
        }
    }

    /// Autonomous-in-policy: a reversible in-policy write AUTO-COMMITS; an irreversible one is HELD.
    #[test]
    fn autonomous_auto_commits_reversible_holds_irreversible() {
        let rev = engine_for(
            insert_plan(),
            qfs_server::Verb::Insert,
            SafetyMode::AutonomousInPolicy,
        );
        assert!(
            matches!(
                commit_plan(&rev, "INSERT ...", false),
                CommitOutcome::Applied(_)
            ),
            "autonomous auto-commits a reversible in-policy write"
        );
        assert!(*rev.applied.lock().unwrap());

        let irr = engine_for(
            remove_plan(),
            qfs_server::Verb::Remove,
            SafetyMode::AutonomousInPolicy,
        );
        assert!(
            matches!(
                commit_plan(&irr, "REMOVE ...", false),
                CommitOutcome::NeedsApproval { .. }
            ),
            "autonomous holds an irreversible write for approval"
        );
        assert!(
            !*irr.applied.lock().unwrap(),
            "the held plan applied nothing"
        );
    }

    /// Approve-everything (most restrictive): it HOLDS even a reversible in-policy write that
    /// Autonomous would auto-apply — the key differentiating behaviour, proven on the real path.
    #[test]
    fn approve_everything_holds_a_reversible_write_on_the_commit_path() {
        let eng = engine_for(
            insert_plan(),
            qfs_server::Verb::Insert,
            SafetyMode::ApproveEverything,
        );
        match commit_plan(&eng, "INSERT ...", false) {
            CommitOutcome::NeedsApproval { reason, .. } => {
                assert!(
                    reason.contains("approve-everything"),
                    "names the mode: {reason}"
                );
            }
            other => panic!("approve-everything must hold a reversible write, got {other:?}"),
        }
        assert!(!*eng.applied.lock().unwrap(), "nothing applied while held");

        // The explicit ack (the card's confirm) satisfies the hold and the write applies.
        let acked = engine_for(
            insert_plan(),
            qfs_server::Verb::Insert,
            SafetyMode::ApproveEverything,
        );
        assert!(matches!(
            commit_plan(&acked, "INSERT ...", true),
            CommitOutcome::Applied(_)
        ));
        assert!(*acked.applied.lock().unwrap());
    }

    /// Policy-only (least restrictive, for CI): it AUTO-COMMITS an irreversible in-policy write
    /// with no per-call ack — the write Autonomous would have held.
    #[test]
    fn policy_only_auto_commits_irreversible_on_the_commit_path() {
        let eng = engine_for(
            remove_plan(),
            qfs_server::Verb::Remove,
            SafetyMode::PolicyOnly,
        );
        assert!(
            matches!(
                commit_plan(&eng, "REMOVE ...", false),
                CommitOutcome::Applied(_)
            ),
            "policy-only auto-commits an irreversible in-policy write unattended"
        );
        assert!(*eng.applied.lock().unwrap());
    }

    /// The FLOOR: an out-of-policy plan is denied in EVERY mode — no preset bypasses the gate, and
    /// the apply is never reached (most-restrictive-wins).
    #[test]
    fn out_of_policy_is_denied_in_every_mode_on_the_commit_path() {
        for mode in SafetyMode::ALL {
            // No explicit allow rule ⇒ default-deny gate refuses the INSERT before the mode runs.
            let eng = FakeEngine {
                plan: Some(insert_plan()),
                mode,
                ..Default::default()
            };
            assert!(
                matches!(
                    commit_plan(&eng, "INSERT ...", true),
                    CommitOutcome::PolicyDenied { .. }
                ),
                "{mode:?} must deny an out-of-policy plan even with ack"
            );
            assert!(
                !*eng.applied.lock().unwrap(),
                "{mode:?} applied a denied plan"
            );
        }
    }
}
