//! The **MCP serving binding** (t47, roadmap M2 — decisions C + K): qfs's JSON-RPC / Model Context
//! Protocol face, the third of "one engine, three faces" (CLI · HTTP · MCP).
//!
//! ## What it is
//! A client LLM (Claude) drives every service qfs fronts through FOUR tools that map 1:1 to qfs's
//! operating loop (roadmap §2.2): **`describe` → `preview` → `commit`**, plus **`connections`** for
//! discovery. This crate is:
//!   1. a PURE JSON-RPC 2.0 + MCP protocol core ([`jsonrpc`] + [`protocol`]): `initialize` /
//!      `tools/list` / `tools/call`, unit-testable against golden wire shapes with NO I/O; and
//!   2. the four tool handlers ([`tools`]) over an injected [`McpEngine`] — the live reads/plans/
//!      applies are supplied by the `qfs` binary, so this crate never touches a live driver; and
//!   3. an [`McpBinding`] ([`binding`]) — a pure, synchronous handler that frames `POST /mcp` into
//!      a JSON-RPC call, composed into the existing in-house HTTP listener by the binary (the
//!      MCP sibling of the t32 qfs-http / t33 qfs-cron / t34 qfs-watchtower serve leaves).
//!
//! ## Decision K — there is NO LLM in qfs
//! This crate has ZERO model/inference dependencies; it only EXPOSES tools. The tool descriptions
//! are prescriptive about WHEN to call each (describe-first, preview-before-commit) — that
//! prescriptiveness is what keeps a capable client model from guessing (roadmap §2.2). Text-to-SQL
//! is the client's job; qfs hosts no model.
//!
//! ## The safety floor is inherited verbatim
//! `describe` is pure (no creds/IO/network); `preview` applies zero effects; `commit` goes through
//! the SAME default-deny policy gate ([`qfs_server::gate_plan`]) and the SAME
//! [`qfs_core::IrreversibleGuard`] the CLI uses — no privileged shortcut, an irreversible plan
//! without ack is REFUSED; `connections` returns names/metadata only, redacted, never secrets.
//! Engine errors are surfaced secret-free (no token/path-secret/stack leak).
//!
//! ## Auth (t50 — the endpoint is gated)
//! [`McpAuthorizer`] is the explicit seam in FRONT of the tool surface. As of t50 the `qfs` binary
//! injects a bearer-validating authorizer that verifies the `Authorization: Bearer <jwt>` access
//! token (signature + `iss`/`aud`/`exp`, against the AS's JWKS) and, on a missing/invalid/expired
//! token, returns a `401` with a `WWW-Authenticate: Bearer resource_metadata="…"` challenge (RFC
//! 9728) so a client discovers the AS and authorizes — only a verified token reaches a tool. The
//! authorizer slots in WITHOUT touching a single handler; the default allow-all [`AllowLocalhost`]
//! is retained only for the inert, localhost-only posture when no AS is configured. The endpoint
//! itself never logs the token (the `Authorization` header is in `qfs_http_core::SENSITIVE_HEADERS`).
//!
//! ## Topology (the leaf guard)
//! qfs-mcp depends on qfs-server (the policy gate) + qfs-exec (the plan/preview) — exactly the
//! role qfs-http/qfs-cron/qfs-watchtower play — and is consumed ONLY by the terminal `qfs` binary.
//! It does NOT depend on qfs-runtime: the apply is the injected [`McpEngine::apply`] closure, so
//! the COMMIT interpreter (and its tokio) stays in the binary. The
//! `mcp_binding_is_a_leaf_serve_consumer` guard pins this.

// Test modules assert/expect/unwrap freely; the strict workspace lint is relaxed under cfg(test).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod binding;
pub mod jsonrpc;
pub mod protocol;
pub mod tools;

pub use binding::{
    AllowLocalhost, AuthDecision, McpAuthorizer, McpBinding, MAX_MCP_BODY_BYTES, MCP_PATH,
};
pub use jsonrpc::{ErrorObject, Request, Response};
pub use protocol::{handle_request, initialize_result, tools_list_result, PROTOCOL_VERSION};
pub use tools::{
    call_tool, commit_plan, default_deny_policy, tool_descriptors, CommitOutcome, ConnectionInfo,
    EngineError, McpEngine, ToolDescriptor,
};

// Re-export the pure HTTP exchange DTOs (qfs-http-core) and the policy type (qfs-server) the
// binding + the `McpEngine` trait speak, so the `qfs` binary's composition root can adapt the
// listener onto them WITHOUT a direct dependency on either crate (its thin-entrypoint guard pins
// the binary off the lower spine; qfs-mcp is the leaf that legitimately binds both).
pub use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};
// The policy type the binding + the `McpEngine::commit_policy` contract speak, plus the policy-rule
// vocabulary an implementor needs to CONSTRUCT one (the binary returns `default_deny_policy()` in
// production, but composing a real `Policy` is part of the trait's contract) — re-exported so the
// `qfs` binary uses them WITHOUT a forbidden direct `qfs-server` edge.
pub use qfs_server::{DriverGlob, Policy, Rule, Verb, VerbSet};
// t81 (roadmap M5, decision U / §3.3): the shared-connection USE gate + the actor-policy DTOs the
// binary's commit-time bind needs to decide whether a member may USE a project/team-owned connection
// — re-exported through this same window so the `qfs` binary consumes them WITHOUT a forbidden direct
// `qfs-server` edge (the binary's thin-entrypoint pin stays intact). `policy_from_def` + `PolicyDef`
// let the binary rehydrate the stored `/sys/policies` grants into a `Policy` to evaluate.
pub use qfs_server::{
    evaluate_shared_use, policy_from_def, Condition, DecisionContext, PolicyDef, RoleGraph,
    ScopeGlob, SharedUseDecision, Subject,
};
