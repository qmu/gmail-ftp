//! The four MCP tools (t47) + the injected [`McpEngine`] seam.
//!
//! The tools map 1:1 to qfs's operating loop (roadmap §2.2): **describe → preview → commit**, plus
//! **connections** for discovery. The handlers here are PURE protocol/shape logic over an injected
//! [`McpEngine`]: the actual reads/plans/applies live behind the trait, supplied by the `qfs`
//! binary's serve composition root (so this crate stays off the live driver / runtime edge).
//!
//! ## The safety floor is inherited verbatim (non-negotiable)
//! - `describe` is PURE — no credentials, no I/O, no network.
//! - `preview` applies ZERO effects — it only builds the plan and renders its dry-run summary.
//! - `commit` routes through the SAME default-deny policy gate ([`qfs_server::gate_plan`]) and the
//!   SAME [`qfs_core::IrreversibleGuard`] the CLI uses. An out-of-policy plan is REFUSED with the
//!   policy decision; an irreversible plan (`REMOVE` / `CALL`) WITHOUT `ack` is REFUSED (a legible
//!   "needs human approval" result), never silently applied. No privileged shortcut exists.
//! - `connections` returns names + metadata only, through the SAME redaction as
//!   `qfs connection list` — never secret material.
//!
//! Upstream engine errors are surfaced as the owned, secret-free [`EngineError`] (code + message
//! only — no token, path-secret, or stack leak), reported in-band as an `isError` tool result so a
//! client model gets a legible failure rather than a transport fault.

use serde::Serialize;
use serde_json::{json, Value};

use crate::jsonrpc::ErrorObject;

/// The four tool names, in operating-loop order. The single source of truth for the descriptors
/// and the `tools/call` dispatch.
pub const TOOL_DESCRIBE: &str = "describe";
/// The preview (dry-run) tool name.
pub const TOOL_PREVIEW: &str = "preview";
/// The commit (apply) tool name.
pub const TOOL_COMMIT: &str = "commit";
/// The connection-list tool name.
pub const TOOL_CONNECTIONS: &str = "connections";

/// An owned, secret-free engine error — the ONLY failure shape the protocol surfaces to a client.
/// Carries a stable `code` (the executor `kind`, e.g. `parse` / `capability` / `commit_failed`)
/// and a secret-free `message`. No path-secret, token, or stack ever crosses this seam.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EngineError {
    /// A stable, coarse error code (the executor `kind` string).
    pub code: String,
    /// A secret-free, machine-facing message.
    pub message: String,
}

impl EngineError {
    /// Build an engine error from a code + message (both must already be secret-free).
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// A generic internal error (used when a result cannot be serialized — should not happen).
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }
}

/// The default-deny [`Policy`](qfs_server::Policy) an unattended / unauthenticated MCP `commit`
/// gates against until a real policy is wired (default-deny is the law — RFD §10). Provided here
/// so the binary's [`McpEngine::commit_policy`] impl can return it WITHOUT depending on qfs-server
/// directly (which the binary's thin-entrypoint guard forbids); qfs-mcp is the one leaf that
/// legitimately binds qfs-server here.
#[must_use]
pub fn default_deny_policy() -> qfs_server::Policy {
    qfs_server::resolve_policy(None, &qfs_server::PolicyTable::new())
}

/// One stored connection — selectors + metadata ONLY (the same shape `qfs connection list`
/// surfaces). Never carries credential material. The binary builds these from the connection
/// store's redacted listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionInfo {
    /// The driver this connection belongs to (e.g. `github`).
    pub driver: String,
    /// The connection name (e.g. `work`).
    pub connection: String,
    /// When the credential was stored (RFC 3339) — plaintext metadata, no secret.
    pub created_at: String,
}

/// The injected engine seam: the live half of each tool, supplied by the `qfs` binary. The
/// protocol core calls THESE; it owns no driver, no runtime, no credential store of its own.
///
/// Implementations MUST honour the safety floor (`describe` pure; `preview` zero-effect; `apply`
/// only ever reached AFTER the gate + guard in [`call_tool`]; `connections` redacted).
pub trait McpEngine: Send + Sync {
    /// The cred-free describe report for `path` (archetype, columns, verbs, procedures, pushdown),
    /// serialized to JSON — exactly what `qfs describe <path>` returns. PURE: no creds, no I/O.
    ///
    /// # Errors
    /// An [`EngineError`] (e.g. `unknown_mount`) when the path resolves to no describe driver.
    fn describe(&self, path: &str) -> Result<Value, EngineError>;

    /// Build the effect [`Plan`](qfs_core::Plan) for `statement` (parse + plan, applies nothing) —
    /// the shared input to both `preview` and `commit`.
    ///
    /// # Errors
    /// An [`EngineError`] on a parse / capability / planning failure.
    fn build_plan(&self, statement: &str) -> Result<qfs_core::Plan, EngineError>;

    /// The [`Policy`](qfs_server::Policy) an MCP `commit` is gated against. Default-deny is the law
    /// for this unattended, unauthenticated surface (no per-statement policy exists yet) — so the
    /// binary supplies a default-deny policy unless a future ticket wires a real one.
    fn commit_policy(&self) -> qfs_server::Policy;

    /// Apply a plan that has ALREADY passed the policy gate + the irreversible-ack guard. This is
    /// the ONLY effecting call; it is the binary's injected runtime-backed commit (qfs-mcp itself
    /// never drives the interpreter — it must stay off qfs-runtime).
    ///
    /// # Errors
    /// An [`EngineError`] (e.g. `commit_failed`) if a leg failed to apply.
    fn apply(&self, plan: &qfs_core::Plan) -> Result<(), EngineError>;

    /// The configured connections — names + metadata only, redacted (never a secret).
    ///
    /// # Errors
    /// An [`EngineError`] if the connection store cannot be listed.
    fn connections(&self) -> Result<Vec<ConnectionInfo>, EngineError>;

    /// The active selectable **safety mode** (t59) this commit path is governed by — resolved by
    /// the binary from the deployment setting (`/sys/settings`, falling back to the env config, then
    /// the safe default). Default impl is [`SafetyMode::AutonomousInPolicy`](qfs_core::SafetyMode):
    /// the safest sensible fallback, so an engine that does not (yet) resolve a mode behaves exactly
    /// as the historical `RunMode::Server` posture (reversible-in-policy auto, irreversible held).
    fn safety_mode(&self) -> qfs_core::SafetyMode {
        qfs_core::SafetyMode::default()
    }
}

/// A single MCP tool descriptor (`tools/list` entry): a name, a prescriptive `when to call`
/// description, and a JSON-Schema for the input arguments.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDescriptor {
    /// The tool name (the `tools/call` selector).
    pub name: &'static str,
    /// A prescriptive description — WHEN to call it (describe-first, preview-before-commit) so a
    /// capable client model drives the loop correctly rather than guessing (roadmap §2.2 / dec. K).
    pub description: &'static str,
    /// The JSON-Schema for the tool's `arguments`.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// The four tool descriptors, in operating-loop order. Pure — a deterministic function of the
/// frozen tool surface (golden-pinned by the wire-shape tests).
#[must_use]
pub fn tool_descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: TOOL_DESCRIBE,
            description:
                "Inspect a path BEFORE writing to it. Returns the node's archetype, columns, \
                 supported verbs, CALL procedures, and pushdown — with NO credentials and NO data \
                 access. ALWAYS call describe first to learn a service's shape before composing a \
                 statement.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "An absolute qfs path, e.g. /mail/drafts or /github/o/r/pulls."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: TOOL_PREVIEW,
            description:
                "Dry-run a qfs statement. Builds the effect plan and returns exactly what WOULD \
                 change, applying ZERO effects. ALWAYS preview an effect statement before commit so \
                 the blast radius (affected counts, irreversible effects) is verified first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "statement": {
                        "type": "string",
                        "description": "A qfs pipe-SQL statement, e.g. INSERT INTO /mail/drafts VALUES (...)."
                    }
                },
                "required": ["statement"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: TOOL_COMMIT,
            description:
                "Apply a qfs statement's effects. Routes through the SAME default-deny policy gate \
                 and irreversible-effect guard as the CLI: an out-of-policy plan is REFUSED with the \
                 policy decision, and an irreversible plan (REMOVE / CALL) is REFUSED unless ack=true \
                 (it then needs explicit human approval). Only call commit AFTER preview.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "statement": {
                        "type": "string",
                        "description": "The qfs statement to apply (the one you previewed)."
                    },
                    "ack": {
                        "type": "boolean",
                        "description": "Explicitly acknowledge an irreversible effect (REMOVE / CALL). Defaults to false; an irreversible plan is refused without it.",
                        "default": false
                    }
                },
                "required": ["statement"],
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: TOOL_CONNECTIONS,
            description:
                "List the configured service connections (driver + name + created-at metadata ONLY, \
                 never secrets). Use to discover which services and accounts are available to \
                 address before describing or writing.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

/// The result of a `tools/call`: MCP tool content (`{content:[{type:"text",text}], isError}`).
/// Built into the JSON-RPC `result` field by [`crate::protocol`].
#[must_use]
fn text_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

/// Render a successful JSON payload as a pretty-printed text tool result (the agent-facing shape).
fn ok_json(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| "{\"error\":\"could not render result\"}".to_string());
    text_result(text, false)
}

/// Render an engine error as an `isError` tool result (in-band, secret-free) — a legible failure
/// for the client model rather than a transport fault.
fn err_result(err: &EngineError) -> Value {
    let body = json!({ "error": { "code": err.code, "message": err.message } });
    let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| err.to_string());
    text_result(text, true)
}

/// Render a REFUSED commit (policy deny or needs-approval) as an `isError` result carrying the
/// stable reason + the secret-free effect summaries — the "needs human approval" / "blocked by
/// policy" signal the client model reads.
fn refused_result(refusal: &str, reason: &str, effects: &[String]) -> Value {
    let body = json!({
        "refused": refusal,
        "reason": reason,
        "effects": effects,
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| reason.to_string());
    text_result(text, true)
}

/// Extract a required string argument by `key` from the `tools/call` arguments object.
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ErrorObject> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| {
        ErrorObject::invalid_params(format!("missing required string argument `{key}`"))
    })
}

/// Dispatch a `tools/call` to the named tool over the injected engine.
///
/// Returns `Ok(result_value)` where `result_value` is the MCP tool result (success OR in-band
/// `isError`), or `Err(ErrorObject)` for a PROTOCOL-level fault (unknown tool, malformed params)
/// that must surface as a JSON-RPC error rather than a tool result.
///
/// # Errors
/// A JSON-RPC [`ErrorObject`] when the tool name is unknown or a required argument is missing.
pub fn call_tool(
    engine: &dyn McpEngine,
    name: &str,
    arguments: &Value,
) -> Result<Value, ErrorObject> {
    match name {
        TOOL_DESCRIBE => {
            let path = required_str(arguments, "path")?;
            Ok(match engine.describe(path) {
                Ok(report) => ok_json(&report),
                Err(e) => err_result(&e),
            })
        }
        TOOL_PREVIEW => {
            let statement = required_str(arguments, "statement")?;
            Ok(match engine.build_plan(statement) {
                Ok(plan) => {
                    // Zero effects: only the dry-run summary of the built plan.
                    let preview = qfs_exec::plan_preview(&plan);
                    match serde_json::to_value(&preview) {
                        Ok(v) => ok_json(&v),
                        Err(e) => err_result(&EngineError::internal(e.to_string())),
                    }
                }
                Err(e) => err_result(&e),
            })
        }
        TOOL_COMMIT => {
            let statement = required_str(arguments, "statement")?;
            let ack = arguments
                .get("ack")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(commit(engine, statement, ack))
        }
        TOOL_CONNECTIONS => Ok(match engine.connections() {
            Ok(conns) => match serde_json::to_value(json!({ "connections": conns })) {
                Ok(v) => ok_json(&v),
                Err(e) => err_result(&EngineError::internal(e.to_string())),
            },
            Err(e) => err_result(&e),
        }),
        other => Err(ErrorObject::invalid_params(format!(
            "unknown tool `{other}`"
        ))),
    }
}

/// The structured verdict of routing a statement through the commit gate + guard (the ONE commit
/// path). Both faces that commit — the MCP `commit` tool here AND the t52 dashboard approval card —
/// call [`commit_plan`] and render THIS verdict (the MCP face as an in-band `isError` tool result,
/// the dashboard as JSON), so neither re-implements the policy/irreversibility decision. Every
/// variant is secret-free by construction: the [`qfs_exec::PlanPreview`] is a dry-run summary, the
/// reason strings are stable sentences, and the effect summaries are `"<VERB> <driver>:<path>"`.
#[derive(Debug, Clone)]
pub enum CommitOutcome {
    /// The plan passed the policy gate + the irreversible-ack guard and the injected applier ran;
    /// carries the committed-apply summary (`committed: true`).
    Applied(qfs_exec::PlanPreview),
    /// The default-deny policy gate REFUSED the plan (out of policy); carries the secret-free deny
    /// reason + the per-effect summaries. NOTHING was applied (the apply is never reached).
    PolicyDenied {
        /// The stable, secret-free policy-denial reason.
        reason: String,
        /// The secret-free per-effect summaries (`"<VERB> <driver>:<path>"`).
        effects: Vec<String>,
    },
    /// The active safety mode (t59) HELD the plan pending an explicit human `ack`: either it carries
    /// an irreversible effect (REMOVE / CALL) under a holding mode, or the *approve-everything* mode
    /// holds even a reversible write. This is the legible "needs human approval" signal the dashboard
    /// maps to its one-time approval card. NOTHING was applied.
    NeedsApproval {
        /// The stable, secret-free needs-approval reason.
        reason: String,
        /// The secret-free per-effect summaries (`"<VERB> <driver>:<path>"`).
        effects: Vec<String>,
    },
    /// An engine error (the plan failed to build, or a leg failed to apply); secret-free.
    Failed(EngineError),
}

/// Route a statement through the commit gate + the selectable **safety mode** (t59) — the SINGLE
/// commit path both the MCP `commit` tool and the t52 dashboard approval card share (no second
/// applier, no privileged shortcut). The order is load-bearing (defence in depth):
///   1. **policy gate** ([`qfs_server::gate_plan`], default-deny) — an out-of-policy plan is
///      refused with the decision; the apply is NEVER reached (zero effects). This is the FLOOR no
///      mode can lower (the mode is consulted only on a plan the gate already allowed).
///   2. **safety-mode decision** ([`qfs_core::IrreversibleGuard::decide`] over the engine's resolved
///      [`SafetyMode`](qfs_core::SafetyMode)) — the live t59 governance. The same in-policy plan
///      yields different outcomes per preset:
///        - *Autonomous-in-policy* (default): reversible auto-commits, irreversible is held for the
///          explicit `ack` (the card's confirm) — the historical `RunMode::Server` posture.
///        - *Approve-everything*: BOTH reversible and irreversible are held for the ack — the most
///          restrictive preset refuses a write Autonomous would auto-apply.
///        - *Policy-only*: BOTH auto-commit (unattended CI) — the mode is the standing ack, but the
///          policy floor in step 1 still denies an out-of-policy plan.
///   3. **apply** — only a plan the mode resolved to auto-commit (reversible-in-policy, or acked, or
///      Policy-only-in-policy) reaches the injected applier.
#[must_use]
pub fn commit_plan(engine: &dyn McpEngine, statement: &str, ack: bool) -> CommitOutcome {
    let plan = match engine.build_plan(statement) {
        Ok(p) => p,
        Err(e) => return CommitOutcome::Failed(e),
    };

    // 1. The default-deny policy gate — the SAME enforcement the cron/watchtower committers use.
    let policy = engine.commit_policy();
    let gate = qfs_server::gate_plan(&policy, &plan);
    if !gate.is_allow() {
        let reason = gate
            .deny_reason()
            .unwrap_or_else(|| "blocked by policy (default-deny)".to_string());
        return CommitOutcome::PolicyDenied {
            reason,
            effects: gate.effects,
        };
    }

    // 2. The selectable safety mode (t59), composed ON TOP OF the gate's allow. `within_policy` is
    //    the gate verdict (true here — a denied plan returned above). The resolved preset decides
    //    whether the in-policy plan auto-commits, is held for an explicit human ack, or (defensively,
    //    out of policy) is denied — irreversibility read solely from the plan, never re-derived.
    let ack = if ack {
        qfs_core::Ack::Granted
    } else {
        qfs_core::Ack::Absent
    };
    let mode = engine.safety_mode();
    match qfs_core::IrreversibleGuard::decide(&plan, mode, gate.is_allow(), ack) {
        qfs_core::SafetyDecision::AutoCommit => {}
        qfs_core::SafetyDecision::NeedApproval => {
            return CommitOutcome::NeedsApproval {
                reason: needs_approval_reason(mode, &plan),
                effects: gate.effects,
            };
        }
        // Unreachable on an allowed plan (within_policy is true), but kept total: a mode never
        // bypasses the policy floor, so a Deny here is surfaced as the policy refusal it is.
        qfs_core::SafetyDecision::Deny => {
            return CommitOutcome::PolicyDenied {
                reason: "blocked by policy (default-deny)".to_string(),
                effects: gate.effects,
            };
        }
    }

    // 3. Apply (through the injected runtime-backed commit). Only reachable for a plan the mode
    //    resolved to auto-commit.
    match engine.apply(&plan) {
        Ok(()) => {
            CommitOutcome::Applied(qfs_exec::PlanPreview::committed(qfs_core::preview(&plan)))
        }
        Err(e) => CommitOutcome::Failed(e),
    }
}

/// The stable, secret-free "needs human approval" reason for a held commit — phrased by WHY the
/// active mode held it (an irreversible effect under any mode that holds it, vs. the
/// approve-everything mode holding even a reversible write), so the agent / approval card reads a
/// legible cause rather than a generic refusal.
#[must_use]
fn needs_approval_reason(mode: qfs_core::SafetyMode, plan: &qfs_core::Plan) -> String {
    if plan.is_irreversible() {
        format!(
            "plan contains an irreversible effect (REMOVE / CALL); the `{mode}` safety mode holds \
             it for explicit human approval (ack)"
        )
    } else {
        format!(
            "the `{mode}` safety mode holds every write for explicit human approval (ack), \
             including this reversible one"
        )
    }
}

/// The `commit` tool body: route the statement through the shared [`commit_plan`] path and render
/// its [`CommitOutcome`] as the MCP tool result (a success payload, or an in-band `isError` for a
/// refusal / engine error). The decision itself lives in `commit_plan` so the dashboard face reuses
/// it verbatim.
fn commit(engine: &dyn McpEngine, statement: &str, ack: bool) -> Value {
    match commit_plan(engine, statement, ack) {
        CommitOutcome::Applied(committed) => match serde_json::to_value(&committed) {
            Ok(v) => ok_json(&v),
            Err(e) => err_result(&EngineError::internal(e.to_string())),
        },
        CommitOutcome::PolicyDenied { reason, effects } => {
            refused_result("policy_denied", &reason, &effects)
        }
        CommitOutcome::NeedsApproval { reason, effects } => {
            refused_result("needs_human_approval", &reason, &effects)
        }
        CommitOutcome::Failed(e) => err_result(&e),
    }
}
