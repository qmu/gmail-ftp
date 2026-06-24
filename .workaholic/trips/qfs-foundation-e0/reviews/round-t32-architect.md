# Round t32 — Architect Analytical Review

- **Reviewer**: Architect (Neutral / structural bridge)
- **Subject**: t32 Server HTTP endpoints — new crate `qfs-http`, ADR-0004, dep_direction guard
  changes, `qfs serve` wiring, `EndpointDef.policy` column.
- **Commit**: `3779188`
- **Mode**: Analytical review only (no test/build/clippy execution — per QA differentiation).

## Decision

**Approve with observations.**

The three headline structural questions resolve cleanly in the Constructor's favour:
the `qfs-http` leaf topology is the principled resolution of the CO-t30-1 / CO-t29-4
collision; the in-house HTTP/1.1 parser is proportionate to E0 and robust enough with the
heavy work parked; and the typed-AST rewrite **is** genuinely injection-safe *for the
documented convention*. The one thing I want flagged to the Lead as a **carry-over (not a
t32 must-fix)** is that the param-naming convention is a latent footgun the moment a param
name collides with a column name — injection-safety holds, but query *semantics* can
silently change. Details below.

---

## Headline Question 1 — `qfs-http` leaf topology + guard changes

**Ruling: this is the principled resolution of the CO-t30-1 / CO-t29-4 collision.**

The collision I flagged earlier was: the HTTP binding must touch BOTH `qfs-server` (the
`Binding`/`ServerState`/`EndpointDef` registry, which CO-t30-1 requires to stay
runtime-free with a *synchronous* owned-snapshot `reconcile`) AND `qfs-exec` (the read
executor, whose consumer set CO-t29-4 had pinned to exactly `{qfs-cmd, qfs}`). Neither
existing crate could legally host it: putting it in `qfs-server` would drag `qfs-exec` (and
its tokio/async surface) below the spine and break the runtime-free invariant; putting it
in `qfs-cmd` would put `qfs-cmd` on `qfs-exec`/`qfs-http`, which the binary-entrypoint guard
forbids.

The Constructor's resolution — a **new leaf crate consumed only by the terminal binary** —
is structurally correct and I verified each load-bearing fact:

- **No spine inversion.** `crates/server/Cargo.toml` has no `qfs-exec` / `qfs-runtime` /
  `qfs-http` edge (confirmed by grep — only a comment noting it is not a runtime consumer).
  `qfs-server` stays runtime-free; `HttpBinding::reconcile` is synchronous, takes an owned
  `&ServerState`, and does the async listener work entirely in `qfs-http` (`serve.rs`).
  CO-t30-1 is intact.
- **`qfs-exec` consumer allowlist extension is a legitimate generalization, not a
  weakening.** The new `http_binding_is_a_leaf_serve_consumer` guard pins the *invariant
  that matters*: the rule was never literally "exec is consumed only by cmd" — it was "exec
  is consumed only by integration leaves at the composition layer, never by a spine/lower
  crate reaching up." `qfs-http` plays exactly the role `qfs-cmd` plays (a leaf integration
  consumer of `execute_read`), and the guard asserts (a) qfs-http depends on both
  qfs-server+qfs-exec, (b) **nothing but the `qfs` binary depends on qfs-http**, (c) qfs-http
  does NOT depend on qfs-runtime. (b)+(c) are the real guarantees; the allowlist string is
  just the surface.
- **tokio still dead-ends.** Because (b) holds, qfs-http's tokio listener is reachable only
  from the terminal `qfs` binary — the same runtime-leaf precondition t28 relied on. tokio
  is declared in `qfs-http` for the HTTP I/O domain (`net`/`io-util`/`sync`/`time`), never
  via `qfs-runtime`, and the COMMIT interpreter is untouched. No spine inversion exists.

**Scaling to t33 (ingest) / t35 (Worker):** the per-binding-leaf pattern scales cleanly. A
t33 ingest binding would be a sibling leaf (`qfs-ingest` or similar) consumed only by the
binary, with its own guard. t35's Worker replaces only `serve.rs` (the wire shim) behind the
same `Binding` seam and the owned `HttpRequest`/`HttpResponse` DTOs — the `EndpointDef → query
→ codec` pipeline is vendor-free, so the Worker reuses everything but the shim. The one thing
the Lead should note for E7: as the number of per-binding leaves grows, the
"only-the-binary-consumes-X" guards multiply; that is acceptable (each is a precise
invariant) but worth a single shared guard helper at t33/t35 to avoid copy-paste drift.

**Observation 1.1**: The allowlist now carries the rationale inline, but the *real* invariant
lives in `http_binding_is_a_leaf_serve_consumer` (the leaf + no-runtime checks). If a future
ticket adds a fourth exec consumer that is NOT a terminal-consumed leaf, the allowlist string
would admit it while the leaf guard would not catch it (the leaf guard is qfs-http-specific).
*Proposal*: at t33, generalize the leaf guard to "every qfs-exec consumer other than the
binary must itself be a binary-only-consumed leaf" so the two guards cannot diverge.

## Headline Question 2 — Injection safety + the param-naming convention (security-critical)

**Ruling: genuinely injection-safe; the convention is a latent footgun → CARRY-OVER to the
grammar/t34 (first-class `:param` node), NOT a t32 must-fix.**

The injection-safety claim holds and is well-constructed:

- The query is parsed **once at registration** (`compile_endpoint` rehydrates the t31
  `StatementSpec` via `from_canonical`, NO re-parse). The request value never re-enters the
  parser (`params::infer_value` produces a typed `Value`; `rewrite::value_to_literal` maps it
  to a single `Literal` leaf). A malicious `'; REMOVE /mail/inbox` becomes one
  `Expr::Lit(Literal::Str(...))` node — data, not structure. `value_to_literal` is **total**
  (the `other =>` arm degrades to a textual literal rather than panicking), so the rewrite
  never panics on an unexpected `Value`.
- The "identical plan for malicious vs benign" property is asserted by the injection golden
  (`tests.rs:384`) — **but only for a non-colliding param** (`q_name` vs columns `id`/`name`).

**The footgun is real and I confirmed it from the rewrite code, not just the doc.**
`rewrite_expr` substitutes EVERY `Expr::Col(ident)` whose identifier is a declared param
(`rewrite.rs:166`). The match is purely by identifier string; it has no way to distinguish a
"param slot" from a genuine column reference. So:

- If an endpoint declares a param whose name **collides with a real column** used in the
  query — e.g. `CREATE ENDPOINT GET /items/:id AS (FROM /mock/items |> WHERE id = id)` — the
  rewrite replaces **both** the LHS column `id` and the RHS slot `id` with the literal,
  producing `WHERE <lit> = <lit>` — a constant predicate. That silently changes query
  semantics (here, all-rows or no-rows depending on the value), and in a richer query could
  **widen access** (e.g. a `WHERE owner = owner` intended to scope to the caller collapses to
  a tautology, removing the scope filter).
- So the "identical plan" guarantee is **conditional on the distinct-name convention**, not
  unconditional. For a colliding param the malicious and benign plans would *still* be
  structurally identical to each other (still a typed literal, so no *injection* in the
  parse-time sense), but **both** would be the wrong plan. The security boundary that does NOT
  break is injection (no DSL parsing of caller data ever happens). The boundary that DOES
  break is **semantic correctness / least-privilege**, silently, with no error.

Why this is a carry-over and not a t32 blocker:
1. The grammar is **frozen** (the doc says so explicitly); there is no `:param` token to
   introduce a first-class bind-parameter AST node within t32's scope. Adding one is a
   grammar/parser change (t34/t35 territory), exactly where the Constructor parks it.
2. The convention is **documented** in `rewrite.rs` and the natural authoring form
   (`:p_id` distinct from `id`) avoids it. At E0, endpoints are authored by the operator, not
   attacker-controlled, so the blast radius is operator-error, not a remote exploit.
3. t32's injection acceptance criterion ("malicious path param does not alter the plan") is
   met for the contract as written.

**Observation 2.1 (must address before t32 is *closed*, cheap, in-scope):** there is **no
guard preventing the footgun from being authored silently.** `compile_endpoint` could, at
registration, reject (or warn) when a declared route param name equals a column name
referenced by the query — a `CompileError::ParamShadowsColumn`. The query AST + the resolved
schema are both available at registration. This converts a silent semantic corruption into a
loud registration refusal *without* needing a grammar change, and it is the t32-appropriate
slice of the larger `:param` fix. *Proposal*: add that registration-time collision check now;
defer the first-class `:param` node to t34/t35.

**Observation 2.2 (test gap):** the injection golden does not cover the collision case. *Proposal*:
add a test asserting that an endpoint whose param name shadows a referenced column is either
refused at registration (preferred, with 2.1) or, at minimum, a documented xfail capturing the
known semantic-change behaviour so a future reader is not surprised.

## Headline Question 3 — In-house HTTP/1.1 server (ADR-0004) as attack surface

**Ruling: acceptable for E0 (native daemon, loopback default) with the heavy work parked.
No must-fix; two robustness observations.**

**Footprint decision (proportionate vs ADR-0002/0003): yes.** ADR-0004 follows the exact
decision shape of ADR-0002 (in-house combine engine over DuckDB) and ADR-0003 (in-house
DEFLATE over a vendor crate): a vendor dep (`axum`) is uncached and disk is at 98%, so the
team uses the already-cached `tokio` and hand-rolls the *minimal* contract behind a
reversibility seam. The seam here is the `Binding` trait + owned `HttpRequest`/`HttpResponse`
DTOs, so an `axum`/`hyper` backend can be added behind a non-default feature later without
touching any caller — the same reversibility ADR-0001/0002/0003 preserved. The scope is
honestly bounded (one request per connection, `Connection: close`, 1 MiB cap, loopback
default, no keep-alive/chunked/TLS/HTTP2, named follow-ups). This is proportionate.

**Parser robustness (I read `serve.rs::read_request` and the helpers line by line):**

- **Malformed request line** → `parse_request_line` returns `None` (needs ≥2
  whitespace-split tokens) → `read_request` returns `Ok(None)` → minimal 400. No panic.
- **Empty request / EOF before headers** → `n == 0` → `Ok(None)` → 400. Handled.
- **Oversized headers** → the header-accumulation loop checks `buf.len() > MAX_REQUEST_BYTES`
  before each read and bails to `Ok(None)`. Bounded.
- **Bad / missing Content-Length** → `.and_then(|v| v.parse().ok()).unwrap_or(0)` — a
  non-numeric or absent value is treated as 0 (no body), not an error. Body read loop also
  re-checks the size bound. No integer-overflow path (parse to `usize`, bounded by the cap).
- **Garbage bytes** → headers are parsed with `String::from_utf8_lossy` (never panics on
  invalid UTF-8); body is raw `Vec<u8>`. `decode`/`percent_decode` use
  `from_str_radix(...).ok()` and `from_utf8_lossy`, so a malformed `%XX` falls back to raw
  text and never panics. Confirmed panic-free on arbitrary input.
- **Request smuggling**: the single-request `Connection: close` model genuinely sidesteps the
  classic CL.TE / TE.CL smuggling class — there is no keep-alive connection reuse and no
  chunked decoding, so there is no second request on the connection to desync. The body is
  read strictly to `Content-Length` and `body.truncate(content_length)` discards any trailing
  pipelined bytes (which are then dropped when the connection closes). This is the correct
  conservative posture for a hand-rolled parser.

**Observation 3.1 (robustness, minor):** `read_request` reads the body up to `content_length`
but **`content_length` itself is unbounded by anything except the per-read `MAX_REQUEST_BYTES`
check inside the loop.** A client can send `Content-Length: 999999999` with a slow/short body;
the loop will keep reading until `body.len() > MAX_REQUEST_BYTES` (1 MiB) then bail — so memory
*is* bounded, good. But there is **no read timeout** on the per-connection future: a client
that opens a connection and sends one byte every minute (slowloris) holds a spawned task
indefinitely. At loopback-default this is not an exploit, but it is a denial vector the moment
`QFS_HTTP_ADDR` is set to a non-loopback bind. *Proposal*: add a per-connection read deadline
(tokio `timeout` around `read_request`) as a small follow-up; the ADR already names "request
deadline" as a federated-latency concern, so fold a connection-read deadline into the same
follow-up and note in ADR-0004's "Out of scope" that non-loopback binds need it.

**Observation 3.2 (correctness, minor):** `find_header_end` scans `buf.windows(4)` on every
read iteration → O(n·m) over the accumulating buffer. Bounded by 1 MiB so not exploitable, but
a single byte-at-a-time sender makes it quadratic within that bound. Acceptable at E0;
*proposal*: note it as a known characteristic, fix only if a real-world client pattern shows
it.

---

## Other surfaces

1. **Read-only policy gate — confirmed real default-deny + plan-assertion at registration.**
   `assert_read_only` (`policy.rs:64`) walks `plan.nodes()` for `is_write_effect` (anything
   not `Read`/`List`); the FIRST write → `PolicyError` UNLESS `policy_grants_writes` (a
   present `PolicyDef` with a non-empty `allow` list — the t34 hook). `None` never grants →
   genuine default-deny. Enforced at registration (`compile_endpoint` → a write-lowering
   endpoint becomes `CompileError::Policy` and is **skipped**, never a route — `binding.rs:116`)
   AND at request time (`dispatch_inner` re-asserts on the bound plan — `handler.rs:133`).
   *Observation*: the t34 hook treats *any* non-empty `allow` as granting *all* writes — a
   coarse over-grant. Acceptable as a documented seam (it is explicitly t34), but the Lead
   should ensure t34 lands before any write endpoint is enabled in a deployment, since today a
   one-entry policy opens the whole write gate. No t32 action.

2. **Query eval reuse — confirmed no new eval logic.** `dispatch_inner` calls
   `qfs_exec::build_plan` + `qfs_exec::execute_read` (`handler.rs:130,136`); the tests use an
   in-memory fake `ReadDriver` (`FakeItems`/`CountingItems`). No bespoke evaluator. Good.

3. **Codec encoding via the registry — confirmed.** `encode_rows` (`encode.rs:73`) resolves
   the codec from `engine.codecs` by format name (`json`/`csv`), rebuilds an owned `RowBatch`,
   and calls `codec.encode`. No serde/axum vendor type past the `http::` boundary (the only
   `serde` use is the owned `ProblemBody` in `error.rs`, which is the error body shape, not a
   data-row leak). 413 guard is enforced **before** codec resolution (`rows.len() > max_rows`).
   Good.

4. **Error mapping / sanitization — confirmed, with one note.** `HttpError` maps
   400/403/404/422/413/500 → JSON `ProblemBody`; `Eval` copies only the executor's already
   secret-free `e.message`; tracing logs route + status + class + **param names only**
   (`route.params`), never values (`handler.rs:90,145`). No token path. *Observation*: the 422
   `eval` body forwards `e.message` verbatim — this relies on the qfs-exec contract that
   `ExecError::message` is sanitized. That contract held at t29 review; flagging that any
   future driver that puts upstream detail into `message` would leak through this 422 body.
   No t32 action; it is the right layering (sanitize at the source).

5. **Hot router swap — confirmed it follows the t30 rule.** `reconcile` rebuilds a fresh
   `Router` from the owned `state` snapshot and swaps `Arc<RwLock<Arc<Router>>>` taking the
   write guard **for the assignment only** (`binding.rs:135`). Readers
   (`handle`/`policies_snapshot`) clone the inner `Arc` under a momentary read guard dropped
   *before* any `.await` (`serve.rs:196`, `handler.rs:67`). A request holds a consistent
   immutable `Router` snapshot for its whole lifetime even while a concurrent reconcile swaps.
   Lock-poison is handled (→ 500, or empty fallback) rather than panicking. The hot-reload test
   (`tests.rs:427`) asserts add→200 / remove→404. Sound.

6. **The two bug fixes — confirmed sound, not masking.**
   - *Single ctrl_c + drain ownership* (`serve.rs:64-112`): the runtime's `run()` OWNS the
     single ctrl_c + audit drain; the listener runs on a separate `watch`-channel shutdown.
     `serve_config` awaits `runtime.run()` to completion (so the drain always runs), THEN
     signals the listener and joins it. This correctly avoids the `select!` race where a
     ctrl_c could drop the drain future un-run. The join error is swallowed so it cannot mask
     the run result (which carries the drain outcome). Correct, not masking — the masked thing
     (listener join error) is genuinely non-load-bearing.
   - *Non-fatal listener bind failure* (`serve.rs:77-101`): a bind error logs a warning and
     boots the config-only runtime rather than aborting. This is a deliberate design choice
     (boot needs no network — RFD §8), well-documented, and observable (the warning). It is NOT
     masking a bug: a port conflict is an expected operational condition, and the audit-drain
     core of `qfs serve` still runs. *Observation*: an operator who *expects* HTTP serving and
     silently gets config-only-on-port-conflict might be surprised. *Proposal*: consider a
     `QFS_HTTP_REQUIRE_BIND=1` opt-in (a later ticket) that makes bind failure fatal for
     deployments where the listener is the point. No t32 action.

7. **`EndpointDef.policy` column — confirmed no t31 regression.** Two distinct structs, both
   consistent: core's `ServerBindingDdl::Endpoint` carries `policy_ref` (the t31 seam, default
   `None`), and `binding_config_row` emits the `policy` column **only when `Some`**
   (`server.rs:503`), so a `None` endpoint omits it and the CREATE≡INSERT golden stays
   byte-identical (the column fills `Null`). The schema adds a **nullable** `policy` column
   (`server.rs:642`). The server-state `EndpointDef.policy` field (`state.rs:57`,
   `#[serde(default)]`) is read back by `driver.rs:310` treating absent/empty as `None`. The
   round-trip is symmetric and the nullable+omit-on-None design preserves the t31 byte-identical
   guarantee. No regression.

8. **Worker portability — confirmed.** All native wire handling is confined to `serve.rs`
   (the ADR's "only native-specific shim"). The pipeline
   (`route`/`params`/`rewrite`/`handler`/`encode`/`policy`/`error`) operates on owned
   `HttpRequest`/`HttpResponse` + `Binding`, with no `tokio`/socket type past `serve.rs`. A
   t35 Worker `fetch` replaces only `serve.rs`. The DTOs are vendor-free. Good.

---

## Cross-cutting structural assessment

The translation fidelity from RFD §8 ("bindings = what causes a plan to run") to the
implementation is high: an HTTP request is modelled exactly as "a cause that makes a (pure
query) plan run," the read-only-by-default gate faithfully represents the §3 purity / §10
least-privilege intent, and the `Binding` seam keeps the business requirement (Worker
portability, E7) traceable through the structure. The footprint ADR is consistent with the
established ADR-0001/0002/0003 pattern, so a stakeholder can trace *why* axum was declined.

The single structural blemish is the param/column identifier overload (Q2): the model
currently represents "a bound parameter" and "a column reference" with the *same* AST node
kind (`Expr::Col`), disambiguated only by an out-of-band naming convention. That is a
fidelity gap — the structure does not faithfully distinguish two semantically different
things — and the clean resolution is the first-class `:param` node at the grammar layer
(t34/t35). The t32-appropriate mitigation is the registration-time collision refusal
(Observation 2.1), which makes the gap *loud* instead of *silent*.

## Summary of proposals (for the Lead)

| # | Concern | Severity | t32 action? |
|---|---------|----------|-------------|
| 2.1 | Param/column name collision silently changes query semantics | Medium (silent, can widen access) | **In-scope cheap mitigation**: registration-time collision refusal. First-class `:param` node → carry-over to t34/t35. |
| 2.2 | No test for the collision case | Low | Add with 2.1. |
| 3.1 | No per-connection read timeout (slowloris on non-loopback bind) | Low at E0 (loopback default) | Follow-up; note in ADR-0004 Out-of-scope. |
| 1.1 | exec-consumer allowlist vs leaf-guard can diverge for a future 4th consumer | Low | Generalize the leaf guard at t33. |
| 6 | Silent config-only fallback on bind conflict | Low | Optional `QFS_HTTP_REQUIRE_BIND` later. |
| 4 | 422 body forwards `ExecError::message` (relies on source sanitization) | Informational | None — correct layering. |
| 1 (other) | t34 hook over-grants (any non-empty policy = all writes) | Informational | Ensure t34 before enabling write endpoints. |

None of these block t32. The leaf topology, injection-safety-for-the-convention, parser
robustness, policy gate, hot swap, the two bug fixes, the schema column, and Worker
portability are all sound.

## Review Notes

Reviewed against `workaholic:design` / `workaholic:implementation` / `workaholic:operation`:
owned DTOs at boundaries, small consumer-side traits, `thiserror` structured errors, codec
registry over ad-hoc serializers, default-deny security posture, reversibility seam for the
vendor decision — all upheld. Analytical review only; no tests/build/clippy executed (Planner
owns E2E, Constructor owns internal).
