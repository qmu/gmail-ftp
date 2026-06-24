# Coding-Phase E2E Review — Planner — t18 (Generic HTTP/REST driver)

- Reviewer: Planner (Progressive)
- Role: E2E / external-interface testing only (no code review, no production code)
- Target ticket: t18 — Driver: generic HTTP / REST (+ `http.get` TVF)
- Method: throwaway external-consumer crate (`/tmp/t18-e2e`, own `[workspace]`, path-deps on
  driver-http/runtime/driver/plan/types/codec/secrets + tokio + tracing-subscriber) driving the
  public API (`RestDriver`/`RestApiConfig`/`AuthStrategy`/`Pagination`/`ReqwestClient`/
  `http_get_node`) through `qfs_runtime::Interpreter::commit` (sync→async `PlanApplierBridge`),
  auth from a `qfs_secrets::InMemoryStore`, responses decoded by `qfs_codec::JsonCodec`.
- Server: in-process LOCAL loopback HTTP/1.1 on `127.0.0.1:0` (`tokio::net::TcpListener`,
  hand-written responses) that records every request (method, path, body, Authorization header)
  and serves canned responses FIFO. **NO live network** — all traffic on the loopback interface.
- Token-leak capture: a `tracing_subscriber::fmt` layer writing into an in-memory buffer; the
  whole run's driver log output (26 KB) is grepped for the token.
- Result: **19 / 19 checks PASS.**

The throwaway crate was removed after the run (no production code touched).

## Verdict: E2E approved

No token leak. No panic on any adversarial input. Retry policy matches the design
(GET retry-eligible, POST never auto-retried). Pagination follows and caps correctly.

---

## PASS / FAIL per task item

### Item 1 — http.get / SELECT → rows — PASS
- `SELECT FROM /rest/api/things` against a JSON-array endpoint decoded to **3 typed rows**
  (`affected = 3`); server recorded exactly one `GET /things`.
- `http.get(<url>, headers=>{Accept=>application/json})` TVF (`http_get_node`) → **2 rows**;
  server recorded one `GET /probe` (no-config probe path, no auth).

Recorded (item 1):
```
GET /things   auth=Bearer <token>   headers=[accept, authorization, host]
GET /probe    auth=None             headers=[accept, host]
```

### Item 2 — POST effect — PASS
- `INSERT INTO /rest/api/things` committed; server received method `POST`, path `/things`,
  and body verbatim `{"name":"new-thing"}` (Content-Length 20).

Recorded (item 2):
```
POST /things  body={"name":"new-thing"}  auth=Bearer <token>  content-length=20
```

### Item 3 — Auth from secret (token safety) — PASS (security-critical)
- (a) **Auth injected**: the loopback server received `Authorization: Bearer <token>` on the
  wire — the secret resolved from the `qfs_secrets` store and was injected into the request.
- (b) **Token absent everywhere the driver could leak it**:
  - `HttpRequest` Debug redacts the value: `("Authorization", "***redacted***")` — token absent.
  - `Outcome`/ledger Debug (178 bytes) — token absent.
  - Per-item driver tracing output (1244 bytes) — token absent.
  - **Whole-run driver log sweep (26 KB) — token absent.**
- Auth-resolution failure (missing secret) surfaces a structured, secret-free terminal error
  `auth resolution failed: secret_not_found` — no panic, no token text.

Token-absent evidence:
```
3b0.request-debug-redacts-token : HttpRequest { ... headers: [("Accept","application/json"),
                                   ("Authorization","***redacted***")], body_len: 0 }
global.no-token-in-logs         : captured 26182 bytes of driver tracing output;
                                   token present = false
```

### Item 4 — Error responses + retry policy — PASS
- **404** → structured **terminal** `client error 404 for GET ...`, `attempts=1`, branchable
  (HttpError::Client → EffectError::Terminal), secret-free, no panic.
- **500 on GET** with `RetryPolicy::new(3, None)` → **retry-eligible**: `attempts=3`, server hit
  **3 times** (transient HttpError::Server → EffectError::Retryable on a retry-safe method).
- **500 on POST** with `RetryPolicy::new(3, None)` → **NOT auto-retried**: `attempts=1`, server
  hit **exactly once** (POST classified terminal via `into_effect_error(method_retry_safe=false)`
  per RFD §6 "never auto-retry POST").

This confirms the design's asymmetry: GET is retry-eligible, POST is not, observed externally
via the ledger `attempts` count and the server hit count.

### Item 5 — Pagination — PASS
- **Cursor**: 3 pages (`next` field → `cursor` param), driver followed all 3; server hits = 3;
  follow URLs observed: `/things` → `/things?cursor=c2` → `/things?cursor=c3`; rows concatenated.
- **Cap enforcement**: server scripted to hand a `next` cursor on **10** pages forever, config
  `max_pages=4` → driver stopped at **exactly 4 hits** (no runaway fetch).
- **Link-header (RFC 5988)**: `rel="next"` followed once; server hits = 2; observed
  `/things` → `/things?page=2`; **3 rows concatenated** (2 + 1).

### Item 6 — Adversarial responses (no panic) — PASS
- **Malformed JSON** (`{not json at all`) → structured terminal `decode error (json): ...`,
  no panic.
- **Empty body** → structured terminal decode error (EOF), no panic; leg dispositioned.
- **Huge body** (100,000-object JSON array, ~1.4 MB) → decoded to **100,000 rows**, no panic,
  no hang.

---

## Concern + proposal (Critical Review Policy)

**Concern (business-outcome / observability):** the run drove the driver's structured request
log at `tracing::debug!` level and confirmed it never carries the token — but the redaction
guarantee currently rests on (1) the manual redacting `Debug` for `HttpRequest`/`HttpResponse`
and (2) the discipline of logging only scalar fields in `send_one`. A future contributor adding
a `tracing::debug!(?req, ...)` or `?headers` line elsewhere in the apply path would bypass the
scalar-field discipline; the redacting `Debug` still saves it, but only because every header
in `SENSITIVE_HEADERS` is matched. A custom `AuthStrategy::Header{name:"X-Tenant-Token"}` whose
name is **not** in `SENSITIVE_HEADERS` would log its value verbatim if ever `?req`-dumped.

**Proposal (constructive, framed as outcome):** to keep the "no credential ever reaches a log"
promise robust as t24/t25 layer on, the team should (a) treat **any** header whose value was
sourced from a `Secret::expose` as sensitive at injection time (tag-on-inject rather than
match-by-name), or (b) add a CI grep that rejects `?req`/`?headers`/`{:?}`-of-request in the
driver-http apply path. This protects the stakeholder-visible promise ("qfs never logs your
tokens") against future header-name configs that the static `SENSITIVE_HEADERS` list does not
anticipate. This is a hardening suggestion, not a blocker — today's auth strategies
(Bearer → Authorization, the built-in `Header` names tested) are all covered and the run shows
zero leakage.

---

## Coverage notes / boundaries respected

- Tested strictly as an **external consumer** through the public crate API and the runtime
  commit path — no internal/unit testing, no code review (Planner QA boundary).
- No live network and no live credentials: loopback server + in-memory secret store only.
- All assertions are reproducible and CLI-runnable (single `cargo run`, exit code reflects
  PASS/FAIL).
