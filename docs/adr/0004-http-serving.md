# ADR 0004 — HTTP serving: in-house minimal HTTP/1.1 over `tokio` vs. `axum`

- **Status**: Accepted (locked)
- **Date**: 2026-06-23
- **Deciders**: qfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t32 — Server HTTP endpoints (`CREATE ENDPOINT` query → live HTTP route): the
  request-driven half of the watchtower server.
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §1 (single, *lean* binary + `wasm32` Workers target), §8
  (bindings = what causes a plan to run; `ENDPOINT` → Worker/route), §9 (no heavy vendor SDKs;
  owned DTOs at the boundary), ADR-0001 / ADR-0002 / ADR-0003 (the same footprint /
  offline-cache / wasm-buildability decision shape — winnow over chumsky, in-house combine
  engine over DuckDB, in-house git reader over gix).

## Decision

**The t32 HTTP serving binding (`qfs-http`) implements a minimal in-house HTTP/1.1 handler over
`tokio::net::TcpListener`. `axum` is NOT taken as a production dependency.** The request/
response pipeline (route match, typed param binding, query eval, codec-encoded response, status
codes) operates entirely on owned, vendor-free DTOs (`HttpRequest`/`HttpResponse`) behind the
generic `qfs_server::Binding` trait. The native wire parse/serialize is the *only*
native-specific shim (`serve.rs`); everything else (`route`/`params`/`rewrite`/`handler`/
`encode`/`policy`/`error`) is vendor-free, so the same `EndpointDef → query → codec` pipeline
maps to a Cloudflare Worker `fetch` handler later (E7/t35) by replacing only that one file.

## Context

The ticket's preferred choice was `axum`. Two facts ruled it out for this trip:

1. **Offline cache miss.** `cargo add axum --dry-run` resolves `axum 0.8.9` from the crates.io
   index, but the actual `.crate` file is **not** in the pre-warmed offline cache
   (`~/.cargo/registry/cache/*/` carries `hyper`, `http`, `http-body`, `tower`, `tower-http`,
   `tower-layer`, `tower-service`, `sync_wrapper` — but **no `axum-*.crate` and no
   `matchit-*.crate`**). The `--offline` dry-run passes only because the INDEX metadata exists;
   a real `cargo build` with an `axum` dep would have to download `axum` + `axum-core` +
   `matchit` + transitive crates from the network.

2. **Disk pressure.** The build volume is at **98 % (≈ 2.1 GiB free)**. Pulling and compiling
   an uncached dependency tree is exactly the footprint risk ADR-0002/0003 were written to
   avoid; the safe-by-default choice is to use what is already cached.

`tokio` IS fully cached (it is already a confined dependency of `qfs-runtime`, `qfs-exec`, and
`qfs-server`), and the endpoint contract is small: method + route match, path/query/body param
extraction, a JSON/CSV body, and a fixed set of status codes. An in-house HTTP/1.1 handler over
`tokio::net::TcpListener` covers it without a new crate — the same trade-off this workspace has
already made for DEFLATE (ADR-0003), SHA-256/HMAC (`driver-objstore`/`driver-slack`), and the
combine engine (ADR-0002).

## Scope of the in-house handler

Implemented exactly to the endpoint contract, no more:

- Request line + headers + a bounded body (`Content-Length`, 1 MiB cap) → owned `HttpRequest`.
- Owned `HttpResponse` → HTTP/1.1 wire bytes with `Connection: close` (one request per
  connection — sufficient for the read-endpoint contract and the loopback tests).
- Loopback bind by default (`127.0.0.1:8787`, `QFS_HTTP_ADDR` override) — RFD §10 trusted bind;
  caller auth is E5/E8.

**Out of scope (named follow-ups):** HTTP keep-alive / pipelining, chunked transfer encoding,
TLS termination, HTTP/2, and a **per-connection read timeout (slowloris defence)**. None are
needed for the t32 query-endpoint contract; should they become necessary, the `Binding` seam
keeps the choice reversible (an `axum`/`hyper` backend could be added behind a non-default
cargo feature without touching any caller — exactly as ADR-0001/0002/0003 kept their vendor
choices reversible behind an owned seam).

**Slowloris / slow-read note.** The in-house `read_request` loop reads header + body up to the
1 MiB size bound, but has **no per-connection read timeout**: a client that opens a connection
and dribbles bytes (or never sends the `\r\n\r\n` terminator) holds a spawned task open. This
is only a vector when the listener is bound to a **non-loopback** address; the t32 default is
loopback-only (`127.0.0.1:8787`, RFD §10 trusted bind), where the attacker would already be on
the trusted host. The mitigation when a deployment binds non-loopback is a `tokio::time::timeout`
around `read_request` (the `time` feature is already enabled) or, preferably, the `hyper`/`axum`
backend behind the reversibility seam above. Recorded as a follow-up; no code at t32.

## Tests require no live network

The request → bind → eval → encode pipeline is tested **in-process** (the `oneshot` analogue):
a `Router::match_request` + `handler::dispatch` call against an in-memory fake `ReadDriver`, no
TCP. The native `serve.rs` listener is the only piece that touches the socket, and it binds
loopback only. No test opens a system port or reaches the network.

## Consequences

- **Pro**: zero new crates; tiny, auditable, wasm-portable; the `Binding` abstraction stays the
  one HTTP boundary so the CF Worker mapping (E7/t35) reuses everything but the wire shim.
- **Con**: a hand-rolled HTTP/1.1 parser is less battle-tested than `hyper`/`axum`; we mitigate
  by bounding request size, closing per request, and confining all wire handling to one module
  with the rest of the pipeline tested in-process. If a richer serving surface is needed, the
  reversibility seam above is the upgrade path.
