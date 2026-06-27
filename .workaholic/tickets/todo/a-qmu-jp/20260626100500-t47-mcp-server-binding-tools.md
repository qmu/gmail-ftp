---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, UX]
effort:
commit_hash:
category:
depends_on: [20260626100000-t42-persistence-sqlite-system-project-db.md]
---

# t47 — MCP server: describe / preview / commit / connections tools

## Overview
Delivers the **MCP endpoint** face of "one engine, three faces": a JSON-RPC MCP server, served
over the existing in-house HTTP listener, that exposes qfs's four operating-loop tools so a
client LLM (Claude) can drive every service qfs fronts. This is roadmap **M2** (Server-as-MCP)
and implements **decision C** (a qfs server IS a remote MCP server) and **decision K** (text-to-
SQL is client-side — qfs never hosts or calls an LLM; it only exposes the tool surface). The four
tools map 1:1 to the existing engine (roadmap §2.2 table): `describe(path)` → `qfs describe`
(pure), `preview(statement)` → `qfs run` (plan only), `commit(statement)` → `qfs run --commit`
(policy + safety gated), `connections()` → connection list (names + metadata only, never
secrets). **What already exists:** the entire read/plan engine (`crates/exec` `build_plan` /
`execute_read`), the credential-free describe registry (`crates/qfs/src/describe.rs`
`describe_registry`), the HTTP listener and its `Binding`/route seams (`crates/http`), and the
connection store (post-t44). **What is genuinely new:** there is NO MCP, NO JSON-RPC, and NO
`qfs-mcp` crate anywhere in the tree — this is the first JSON-RPC surface qfs has. This ticket
builds the unauthenticated tool surface; **bearer/OAuth auth in front of it is t50**, and the
OAuth AS it depends on is t48–t49. Until then the endpoint MUST default to localhost-only.

## Exact seams
- **New crate `qfs-mcp`** (pure-ish core + a `Binding` impl): JSON-RPC 2.0 framing, the MCP
  `initialize` / `tools/list` / `tools/call` methods, and the four tool handlers. The protocol
  framing and tool-schema construction are pure and unit-testable; the actual reads/plans are
  delegated through injected closures so the crate stays off the live-driver dep edge.
- `crates/exec/` — `build_plan()` (statement → `Plan`, for `preview`) and `execute_read()` (the
  read-path executor, for `describe` data and for read previews). `commit` drives the runtime
  apply path (`run_oneshot`/`apply_commit`) the CLI's `qfs run --commit` already uses — reuse it,
  do not reimplement plan/commit.
- `crates/qfs/src/describe.rs` `describe_registry()` — the cred-free describe registry; the
  `describe(path)` tool returns exactly what `qfs describe` returns (archetype, columns, verbs,
  `CALL` procedures, pushdown). Pure, no credentials, no network.
- `crates/http/src/binding.rs` `HttpBinding` (arc-swap hot route table), `src/route.rs`
  `Router`/`RoutePattern`/`compile_endpoint`, `src/handler.rs` `dispatch`, and the `Fallback`
  closure seam in `serve.rs` — mirror `HttpBinding` to mount the MCP JSON-RPC endpoint (e.g.
  `POST /mcp`) on the same `tokio::net::TcpListener`. No axum; use the existing `http-core` DTOs.
- `crates/http/src/policy.rs` `assert_read_only` + `crates/server/src/policy/enforce.rs`
  `evaluate(policy, plan) -> PolicyDecision` (default-deny, pure) and `gate.rs` `gate_plan` — the
  `commit` tool MUST route through the same policy gate and the safety guard, never a shortcut.
- `crates/core/src/security.rs` `IrreversibleGuard::require_ack(plan, mode, ack)`, `RunMode`,
  `NeedsPreview` — `commit` honors the irreversible-ack rule; an irreversible plan from a tool
  call is NOT auto-applied (the selectable safety mode is t59; until then default = require ack /
  refuse irreversible over MCP and say so in the tool result).
- Connection store (post-**t44**: `crates/secrets` `ConnectionId`/`AccountRecord`/`resolve`, the
  `connection list` path) — `connections()` returns names + metadata only; it MUST go through the
  same redaction as `qfs connection list` and never read secret material.
- `crates/qfs/src/serve.rs` (`run_serve` → `qfs_http::serve_config_full`) — composition root;
  wire the new MCP binding alongside `CronBinding`/`WatchtowerBinding`, injecting the engine
  closures (build_plan/execute_read/commit/describe/connection-list).
- `crates/cmd/tests/dep_direction.rs` — add `qfs-mcp` to allowlists; live driver/runtime edges
  land only on `crates/qfs`. Keep tokio confined to the binding + runtime, not the pure core.

## Implementation steps
1. **JSON-RPC + MCP framing (pure).** In `qfs-mcp`, implement JSON-RPC 2.0 request/response/error
   types and the MCP handshake: `initialize` (advertise protocol version + capabilities),
   `tools/list` (return the four tool descriptors with JSON-Schema input + *prescriptive* `when
   to call` descriptions), and `tools/call` dispatch. All pure; unit-test the wire shapes against
   golden fixtures. No I/O here.
2. **Tool handlers over injected closures.** Define a `McpEngine` trait (consumer-side) with
   `describe(path)`, `preview(statement)`, `commit(statement, ack)`, `connections()`. Implement
   the handlers to call it and shape results as MCP tool content. The crate depends on NO live
   driver — the impl is injected from the binary.
3. **Binding + route.** Add an `McpBinding` mirroring `crates/http` `HttpBinding`: a `POST /mcp`
   route whose handler frames request body → `tools/call` → response, using `http-core` DTOs and
   the arc-swap pattern. Reject non-POST / wrong content-type with structured JSON-RPC errors.
4. **Wire the engine in the binary.** In `crates/qfs/src/serve.rs`, implement `McpEngine`:
   `describe` → `describe_registry()`; `preview` → `qfs_exec::build_plan` + `preview`; `commit` →
   the existing `run_oneshot`/`apply_commit` path *through* the policy gate
   (`server::policy::gate_plan`) and `IrreversibleGuard`; `connections` → the t44 connection-list
   path. Default-bind localhost; document that auth (t50) is not yet present.
5. **Safety + redaction tests.** Golden tests: `describe` returns the same contract as
   `qfs describe` with no credentials; `preview` returns a plan with zero effects applied;
   `commit` of a reversible in-policy plan applies, an out-of-policy plan is refused with the
   policy decision, an irreversible plan without ack is refused (not applied); `connections`
   never includes secret material (assert against redaction). All hermetic, no network.
6. **Docs + version.** Add `qfs-mcp` to `dep_direction.rs`. Update the roadmap status tag for §2.2
   only as far as truly shipped (the MCP *tools* exist; auth does not — say so). DO NOT advertise
   "Claude can connect" until t48–t50 land. `cargo run -p xtask -- gen-docs --check`; patch-bump
   `crates/qfs/Cargo.toml`.

## Key files
- `crates/mcp/` (new): `Cargo.toml`, `src/lib.rs`, `src/jsonrpc.rs`, `src/protocol.rs`
  (initialize/tools/list/tools/call), `src/tools.rs` (the four handlers + `McpEngine` trait),
  `src/binding.rs` (`McpBinding`).
- `crates/qfs/src/serve.rs` (modify): implement + inject `McpEngine`, wire `McpBinding`.
- `crates/qfs/src/describe.rs` (reuse): `describe_registry()` for the `describe` tool.
- `crates/cmd/tests/dep_direction.rs` (modify): allowlist `qfs-mcp`.
- `crates/qfs/Cargo.toml` (modify): patch bump.

## Considerations
- **Safety floor is inherited verbatim.** `describe` is pure (no creds/IO/network); `preview`
  touches nothing; `commit` is explicit and goes through the SAME policy gate
  (`enforce::evaluate`, default-deny) and the SAME `IrreversibleGuard` the CLI uses — an MCP tool
  call gets no privileged shortcut. Irreversible plans over MCP are refused (ack required) until
  the selectable safety mode (t59) defines auto-commit-in-policy; document this clearly in the
  tool result so the client LLM gets a legible "needs human approval" signal.
- **Decision K — no LLM in qfs.** The crate has zero model/inference dependencies; it only
  *exposes* tools. Tool descriptions are prescriptive about WHEN to call each (describe-first,
  preview-before-commit) — that prescriptiveness is what keeps a capable client model from
  guessing (roadmap §2.2).
- **No secrets over the wire.** `connections()` returns names/service/metadata only, through the
  same redaction as `qfs connection list`; never secret material. Error bodies sanitize upstream
  engine errors — no token, path-secret, or stack leak to the client.
- **Auth gap, stated honestly.** This endpoint is UNAUTHENTICATED this ticket; it MUST default to
  a localhost bind and the docs/skill MUST NOT claim a remote MCP connection works until t50 puts
  bearer/OAuth in front of `McpBinding`. Leave the middleware seam explicit so t50 slots in
  without touching tool logic.
- **Dep-direction.** `qfs-mcp` core is pure (no live driver, tokio only in the binding); the live
  engine closures are injected from `crates/qfs`. Add the crate to `dep_direction.rs`.
- **Open product decisions to flag.** (a) Transport: streamable-HTTP MCP vs. SSE vs. plain
  POST/response — pick the current MCP spec's HTTP transport; flag if the spec revs. (b) Whether
  `preview` and `commit` are two tools or one tool with a `commit: bool` arg — the roadmap table
  lists them separately, so ship two; note the choice. (c) Result pagination / max rows for large
  reads (mirror the `crates/http` bounded-buffer concern).
- **Versioning.** One PR, one patch bump, a `v0.0.x` tag on ship.
