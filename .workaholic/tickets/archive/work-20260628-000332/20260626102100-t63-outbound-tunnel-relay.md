---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: M
commit_hash: 9b74ea5
category: Added
depends_on: [20260626100800-t50-bearer-refresh-token-mcp-auth.md]
---

# t63 — qfs-native outbound tunnel + relay

## Overview

Implements the **agent fabric transport** for M7 (roadmap §3.3, decisions L and N): each machine
runs a resident `qfs` that joins the fleet over a qfs-native **outbound** tunnel relayed by qfs
Cloud, so neither the office desktop nor a home laptop ever opens an inbound port. This ticket
builds only the transport + relay seam and its authentication gate; the `/claude/...` driver that
rides it lands in t64. Nothing of the tunnel exists today — this is genuinely new. What exists to
build on: the in-house HTTP/1.1 listener (`crates/http/src/serve.rs`, no axum) that already accepts
local connections, the pure HTTP DTOs in `crates/http-core` (`HttpRequest`/`HttpResponse` +
`SENSITIVE_HEADERS` redaction), the synchronous runtime-free `HttpExchange` seam used by
`crates/google-auth` for outbound calls, and the bearer-token validation that t50 puts in front of
the MCP binding. The relay reuses that same bearer identity — every cross-machine call is
authenticated by the t45/t50 identity and bounded by `POLICY` (decision N: using the tunnel
requires a qfs Cloud sign-in).

## Exact seams

- `crates/http/src/serve.rs` — the in-house HTTP/1.1 server over `tokio::net::TcpListener` (no
  axum); the resident node already runs one. The tunnel is a NEW outbound counterpart that *dials*
  the relay rather than listening, so the resident node needs no inbound port.
- `crates/http-core/src/lib.rs` — pure `HttpMethod`/`HttpRequest`/`HttpResponse` DTOs and the single
  `SENSITIVE_HEADERS`/`is_sensitive_header` redaction authority; the tunnel frames carry these DTOs
  so the redaction floor is inherited, not re-implemented.
- `crates/google-auth/src/lib.rs` — the existing synchronous `HttpExchange` seam (network rides a
  runtime-free exchange trait) is the pattern to mirror for the resident node's outbound dial to the
  relay; do NOT invent a second transport abstraction.
- `crates/qfs/src/transport.rs` — the ONE real `HttpTransport`; the relay-dial transport is a NEW
  binary-leaf sibling here (tokio lives in the leaf, not in any pure core).
- t50 bearer-token validation (in front of the `qfs-mcp` binding) — the relay authenticates every
  inbound-over-tunnel call against the same token→user mapping; the tunnel adds no second auth model.
- `crates/server/src/policy/enforce.rs` `evaluate(policy, plan) -> PolicyDecision` (pure,
  default-deny) — cross-machine calls pass through the same gate; the tunnel widens reach, never
  authorization.
- `crates/cmd/tests/dep_direction.rs` — `TERMINAL_LEAVES = ["qfs","qfs-skill","xtask"]`; the new
  `qfs-tunnel` (pure framing/protocol core) is added to the runtime-consumer allowlist, and its
  tokio dial half lands on `crates/qfs`.

## Implementation steps

1. New crate `qfs-tunnel` (pure, tokio-free leaf): define the wire protocol DTOs — a frame envelope
   (`TunnelFrame { stream_id, kind, body }`), `FrameKind { Open, Data, Close, Ping }`, and the
   request/response carried inside (`crates/http-core` `HttpRequest`/`HttpResponse`, so redaction is
   inherited). Serde + a length-prefixed codec; no I/O. Add to the dep_direction allowlist. Tree
   stays green (pure unit tests only).
2. Define the relay handshake as data: a `RelayHello { node_id, cloud_token }` and `RelayAccepted {
   session_id }`. Validation of `cloud_token` reuses the t50 token-validation seam (passed in as a
   closure/trait, not duplicated) so decision N — qfs Cloud sign-in required — is enforced at the
   single existing identity point. Pure tests: a hello with no/expired token is rejected.
3. Resident-node outbound dial (binary leaf `crates/qfs/src/tunnel.rs`, NEW): mirror
   `crates/qfs/src/transport.rs` to open ONE long-lived outbound connection to the relay using the
   `HttpExchange`-style seam, send `RelayHello`, then service inbound `Open` frames by dispatching
   the carried `HttpRequest` to the *local* `crates/http` handler path. No inbound listener is
   opened — this is the "machines never open a port" guarantee.
4. Relay side (binary leaf, `crates/qfs/src/relay.rs`, NEW, behind a `qfs serve --relay` mode):
   accept resident-node connections on the qfs Cloud listener, validate `cloud_token` (t50), keep a
   `node_id → session` table, and forward a caller's framed request to the addressed node. Caller
   identity is the t50 bearer; the relay forwards it so the destination node re-checks `POLICY`.
5. Policy + redaction wiring: every forwarded request is gated by
   `crates/server/src/policy/enforce.rs` `evaluate` at the destination (default-deny); every frame
   logged goes through `http-core` `is_sensitive_header`. Add an audit entry per cross-machine call
   via `crates/server/src/audit.rs` `AuditSink`.
6. Docs + version: keep the tunnel out of `docs/`, the skill, and the README until a live two-node
   smoke passes (honesty-first). Patch-bump `crates/qfs/Cargo.toml`. Update generated docs only if a
   surface actually shipped (`cargo run -p xtask -- gen-docs --check`).

## Key files

- `crates/tunnel/src/lib.rs`, `crates/tunnel/src/frame.rs`, `crates/tunnel/src/handshake.rs` (new
  pure crate).
- `crates/qfs/src/tunnel.rs` (new — resident outbound dial), `crates/qfs/src/relay.rs` (new — relay
  forwarder), wired from `crates/qfs/src/serve.rs`.
- `crates/cmd/tests/dep_direction.rs` (add `qfs-tunnel` to the allowlist).
- `crates/qfs/Cargo.toml` (patch bump).

## Considerations

- **Safety floor inherited, not re-invented.** A request arriving over the tunnel is still a qfs
  statement: describe is pure, preview touches nothing, commit is explicit, irreversible needs the
  extra acknowledgement. The destination node runs the SAME `crates/http`/`qfs_exec` path a local
  call does, so the floor cannot be bypassed by coming in over the wire. The tunnel adds *reach*,
  never a new capability (one-engine-three-faces).
- **Decision N is the gate.** Using the tunnel REQUIRES a qfs Cloud sign-in; enforce it at the relay
  handshake against the t50 token-validation seam — do not add a parallel auth path.
- **Outbound-only is the security property.** The resident node never opens an inbound port; it only
  dials out. Keep it that way — no convenience "direct mode" inbound listener without a separate,
  explicitly-acknowledged decision.
- **Dep-direction.** `qfs-tunnel` stays pure/tokio-free; all dialing/forwarding tokio lives in
  `crates/qfs` (the terminal leaf where tokio dead-ends). Add the new crate to
  `crates/cmd/tests/dep_direction.rs`.
- **Redaction.** Frames may carry credentials in headers/bodies; route every log line through
  `crates/http-core` `is_sensitive_header` (the single authority) — never log raw frames.
- **Open product decision to FLAG, not guess:** the relay's addressing/discovery model (how a caller
  names `acme-ci`), the qfs Cloud relay's own hosting (Workers vs. a long-lived process), and
  reconnect/backoff semantics on relay restart. Name these in the PR; do not bake them in.
- **Versioning:** own PR + patch bump in `crates/qfs/Cargo.toml` (currently 0.0.7) + a `v0.0.x` tag
  on ship.
