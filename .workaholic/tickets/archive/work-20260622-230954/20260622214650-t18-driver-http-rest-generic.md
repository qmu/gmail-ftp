---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash: d6d39fb
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t15-codec-registry-decode-encode.md]
---

# Driver: generic HTTP / REST (+ http.get TVF)

## Overview

This ticket delivers the **escape hatch** for any service cfs does not model natively: a
*configured* REST driver mounted at `/rest/<api>/...` and an ad-hoc `http.get(url, headers=>…)`
table-valued function. It implements the driver contract (RFD §5), the open-paths registry
(RFD §3), and the "generic REST" backend named in the vision (RFD §1). The REST driver maps
the **universal CRUD verbs** onto HTTP methods *internally* — `SELECT→GET`, `INSERT→POST`,
`UPSERT→PUT`, `REMOVE→DELETE` — so the DSL stays closed-core: there are **no HTTP-verb
keywords**, the path is the type (RFD §3 "universal verbs vs domain actions"). Auth, headers,
base URL, and pagination are *config*, not grammar. Combined with the codec registry
(`DECODE json`), an agent can read/write an arbitrary JSON API with zero new keywords and a
small TOML config. `http.get` is the no-config probe for one-off requests.

## Scope

In scope:
- A `RestDriver` implementing the `Driver` trait (t13), mounted as `/rest/<api>/...`.
- Config-driven instances: base URL, auth strategy, static/templated headers, pagination
  strategy, path→resource mapping. One config block per `<api>`.
- Verb→method mapping (`SELECT/INSERT/UPSERT/REMOVE`) emitting effect-plan nodes; reads are
  pure, writes evaluate to `Plan` (RFD §3 purity invariant).
- Pagination drivers: `none`, `cursor` (next-token field), `link-header` (RFC 5988), `page`/`offset`.
- Response bodies handed to the codec registry (default `DECODE json`) → rows.
- `http.get(url, headers=>{...})` TVF registered in the function registry; one GET → rows.
- Capability declaration + parse-time rejection of unsupported verbs per node.

Out of scope (deferred):
- OAuth2 token acquisition/refresh & encrypted credential store → **E5 / t-auth**. This ticket
  consumes resolved secrets via a `CredentialResolver` handle; it does not store or refresh them.
- Pushdown of `WHERE`/`ORDER BY` into query strings beyond a thin documented mapping →
  **t-federation-pushdown** (E3). Here, only trivial passthrough params.
- `UPDATE` (PATCH) and partial-update semantics — REST PATCH varies wildly per API; deferred to
  a follow-up REST-PATCH ticket. This ticket does `SELECT/INSERT/UPSERT/REMOVE` only.
- GraphQL / SOAP / gRPC backends.

## Key components

New crate-internal module `drivers/rest/` (in the `cfs-drivers` crate):
- `RestDriver { instances: HashMap<String, RestApiConfig> }` — implements `Driver` (t13).
- `RestApiConfig` (deserialized from cfs config, owned DTO, no vendor types):
  ```rust
  pub struct RestApiConfig {
      pub base_url: Url,
      pub auth: AuthStrategy,            // Bearer{secret_ref}, Header{name,secret_ref}, None
      pub default_headers: Vec<(String, String)>,
      pub pagination: Pagination,        // None | Cursor{next_field, param} | LinkHeader | Page{...}
      pub default_codec: CodecId,        // default DECODE json
      pub resources: Vec<ResourceMap>,   // path segment -> {supported verbs, id field}
  }
  ```
- `enum AuthStrategy`, `enum Pagination` — closed sum types (RFD §9 enums for capabilities).
- `trait Driver` methods touched: `describe(path)` (archetype = Relational/table or Append/log
  per resource; schema = open struct since JSON is dynamic), `capabilities(path)`,
  `plan_select/plan_insert/plan_upsert/plan_remove(path, rows) -> Plan`.
- Effect node: `HttpEffect { method, url, headers, body: Option<Bytes>, idempotency_key:
  Option<String>, irreversible: bool }` — a `Plan` leaf the interpreter (E2) applies.
- `HttpClient` — a **thin** `reqwest`-based client (RFD §9 "no heavy vendor SDKs"); behind a
  trait so a mock client is injected in tests; `wasm32` uses the Workers `fetch` shim.
- `http_get` TVF: registered in the function registry as `http.get(url, headers=>map) -> Plan`
  that yields a pure read producing rows via the codec registry.
- Secrets are referenced by `secret_ref` and resolved through the injected `CredentialResolver`;
  raw values never appear in config structs or logs.

## Implementation steps

1. Add `drivers/rest/` module; define `RestApiConfig`, `AuthStrategy`, `Pagination`,
   `ResourceMap` with `serde` deserialization from the cfs config format.
2. Define `HttpClient` trait + `reqwest` impl (foreground) and a `MockHttpClient` (tests);
   feature-gate the wasm `fetch` impl.
3. Implement `Driver::describe`/`capabilities`: map each `ResourceMap` to an archetype and the
   verbs it supports; reject unsupported verbs at parse time with a structured error.
4. Implement `plan_select` → builds `HttpEffect{GET}` + pagination expansion (a plan that, when
   committed, follows cursors/links and concatenates pages), pipes response bytes through the
   configured codec to rows.
5. Implement `plan_insert(POST)`, `plan_upsert(PUT)`, `plan_remove(DELETE)`; set
   `irreversible=true` for `REMOVE`; attach an `idempotency_key` for `UPSERT`.
6. Wire `AuthStrategy` + `default_headers` into request construction, pulling secrets via
   `CredentialResolver` at commit time (not plan-build time).
7. Register `http.get` TVF in the function registry; resolve `headers=>{...}` named arg to a
   header list; reuse the same codec-decode path.
8. Register `RestDriver` in the driver/paths registry so `/rest/<api>/...` mounts resolve.
9. Tests: golden plan tests + mock-HTTP integration tests (below).
10. `cargo fmt`, `cargo clippy -D warnings`, `cargo test`.

## Considerations

- **Least privilege & secrets** (RFD §10): secrets are `secret_ref` indirections resolved by the
  injected resolver at commit; config and `Debug`/log output must never contain token material —
  add a `redacted` newtype or manual `Debug`. Capability gating means a `POLICY` can scope which
  `/rest/<api>` mounts a server handler may touch.
- **Idempotency / recovery** (RFD §6): `UPSERT→PUT` is the retry-safe path for at-least-once
  webhooks; carry an idempotency key. `REMOVE→DELETE` is `irreversible` — it only runs under
  explicit `COMMIT`, and lands in the audit ledger. `INSERT→POST` is *not* idempotent; document
  this and do not auto-retry POST on ambiguous (timeout) failures.
- **The genuinely hard part — pagination as pure plan vs. streaming I/O.** Page-following is
  inherently sequential I/O (next cursor depends on prior response), which fights the
  "construct a static DAG up front" model. Resolve by emitting a single `HttpEffect` carrying the
  `Pagination` policy and letting the *interpreter* (E2) drive the follow loop at the edge; the
  plan stays pure/previewable (PREVIEW shows "GET … (paginated: cursor)"), the loop is an
  interpreter concern. Bound max pages to avoid runaway fetches.
- **Dynamic JSON schema**: REST responses are weakly typed; `describe` returns an open struct
  archetype and lets `DECODE json` produce struct columns (RFD §4 — irregular JSON stays a
  struct column). Do not invent column types.
- **Observability** (RFD §6): per-request timeout, bounded retries (GET/PUT/DELETE only, never
  POST), circuit breaker per `<api>`, structured logs with URL + status but redacted headers.
- **Coding standards / boundaries** (RFD §9): `reqwest`/`Url` types must not leak past the driver
  boundary — rows out, `RestApiConfig`/`HttpEffect` are owned DTOs.

## Acceptance criteria

- `cargo build`, `cargo clippy -D warnings`, `cargo test` all green; wasm feature compiles.
- `RestDriver` registered; `/rest/<api>/...` paths resolve and `DESCRIBE` returns archetype +
  capabilities for configured resources.
- **Plan assertions (no live creds)** via `MockHttpClient`:
  - `SELECT FROM /rest/<api>/things` builds a plan whose leaf is `HttpEffect{GET, <base>/things}`
    with configured auth header present (value redacted in any logged form).
  - `INSERT INTO /rest/<api>/things VALUES(...)` → `POST`; `UPSERT` → `PUT` with idempotency key;
    `REMOVE` → `DELETE` with `irreversible=true`.
  - A verb not declared for a resource is rejected at **parse time** with a structured error.
- **Golden tests**: PREVIEW output for each verb matches a checked-in golden (method, URL,
  redacted headers, pagination note).
- Pagination test: mock returns 3 cursor pages → committed `SELECT` yields concatenated rows;
  page cap is enforced.
- `http.get('https://…/x', headers=>{Accept=>'application/json'})` returns rows decoded via the
  json codec against `MockHttpClient`.
- No secret material appears in `Debug` output or logs (asserted by a redaction test).
