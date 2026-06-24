# Round t25 — Architect Analytical Review

- Reviewer: Architect (Neutral / structural bridge)
- Subject: t25 Slack driver, commit `c5cb1ab` — new crate `crates/driver-slack/`
- Mode: analytical review only (no build/test/clippy executed — read + reason)
- Decision: **Approve with minor suggestions**

---

## Scope reviewed

All 13 source modules of `crates/driver-slack/` (`lib`, `client`, `effect`, `applier`,
`events`, `hmac`, `path`, `procs`, `pushdown`, `read`, `schema`, `dto`, `error`), the
`Cargo.toml` feature graph, the `tests.rs` (1085 lines), and the workspace
`dep_direction.rs` allowlist append. Assessed against the t25 ticket and the nine
high-value surfaces the Lead named.

---

## 1. wasm feature-gating — the structural precedent (the headline call)

**Ruling: structurally clean, and it is the right precedent for the driver layer — adopt it.**

The split is `default = ["runtime"]`, `runtime = ["dep:qfs-runtime"]`, with the optional
`qfs-runtime` dependency the *only* thing the feature gates. What makes this clean rather
than `#cfg`-rot:

- **The cut is at a single, real seam, not scattered.** Exactly two `#[cfg(feature =
  "runtime")]` sites exist: the free function `slack_apply_driver` (`lib.rs:274`) and the
  `runtime_bridge` *module* (`applier.rs:63`). The pure subset — `parse_event`, `hmac`,
  `path`, `schema`, `dto`, `effect`, `procs`, `pushdown`, `read`, the introspective
  `Driver` impl, and the synchronous `PlanApplier` impl — carries **no** cfg attribute. The
  feature gates a *capability* (the async bridge), not a sprinkling of conditional fields.
- **No runtime type leaks into the pure subset.** `qfs_runtime::{SharedApplier, EffectError,
  EffectOutput, PlanApplierBridge}` appear only inside the gated module/function. The
  synchronous `PlanApplier for SlackApplier` (`applier.rs:47`) is feature-independent and
  references only `qfs_plan` types — so the introspective + apply core is genuinely closed
  over pure leaves (`qfs-driver`, `qfs-plan`, `qfs-types`, `qfs-secrets`, `qfs-http-core`,
  serde). None of those pull tokio.
- **The closure is real, not asserted.** `qfs-http-core` is independently pinned as a pure
  leaf carrying no reqwest/tokio (`dep_direction.rs:422`), and the Slack transport is a
  *local* `HttpTransport` trait (`client.rs:52`) rather than a dep on `qfs-driver-http`, so
  the events-only build's dep closure provably excludes the runtime. The Constructor's
  reported verification (events-only links no tokio; default build correctly fails on wasm)
  is structurally consistent with what I read.

**Why it is the right precedent for the parked t22–t24 wasm entrypoints.** The pattern's
load-bearing property is *the impure bridge is the leaf's only runtime coupling, and it is
the thing gated off*. Every driver in this layer has that exact shape (a synchronous
`PlanApplier` core + a `PlanApplierBridge`-driven async bridge). So the same one-feature cut
transplants with no per-driver cleverness: gate `dep:qfs-runtime` + the `runtime_bridge`
module + the `*_apply_driver` fn, leave everything else feature-free. I endorse standardizing
this as the **driver-layer wasm convention**.

**Concern (minor): the `events` feature is a no-op marker and that is a latent trap.**
`events = []` does nothing — `--no-default-features` alone already excludes the bridge
(`Cargo.toml:17-19` says as much). A marker feature that gates nothing invites a future
contributor to add a *second* runtime-touching item and forget that `events` does not
actually fence it; the only real fence is "absence of `runtime`". **Proposal:** either (a)
make the wasm subset's integrity *mechanical* — add a CI/dep-direction-style assertion that
the `--no-default-features` build of each driver excludes tokio (a structural twin of the
existing `runtime_is_confined_to_plan_and_types` test, but for the leaf's own feature
graph) — or (b) at minimum document on the `events` feature that it is presentational and
the true gate is `not(feature="runtime")`. (a) is the durable version and is the artifact
the precedent should ship with, so the convention is enforced rather than conventional.

---

## 2. HMAC-SHA256 signature verification + constant-time compare

**Confirmed correct on every point the Lead named.**

- **Base string is exactly `v0:{timestamp}:{body}`.** `verify_signature` (`events.rs:258-263`)
  builds `SIG_VERSION` + `:` + `ts_str` + `:` + `body` from bytes. Critically it uses the
  **raw `ts_str`**, not a re-rendered parse of `ts` — so a timestamp like `01700…` would sign
  over its verbatim form (Slack uses the verbatim header), and `body` is the exact request
  bytes, never a re-serialized JSON (the doc comment calls this out and the code honors it).
  Correct.
- **Constant-time compare is genuine.** `constant_time_eq` (`hmac.rs:215-226`) folds
  `a.len() ^ b.len()` into the accumulator, scans `max(len_a, len_b)` with
  `get(i).unwrap_or(0)`, ORs every byte diff, and only tests `diff == 0` at the end. No early
  return on length mismatch, no first-mismatch short-circuit. It compares the hex *strings*
  (`expected.as_bytes()` vs `sig.as_bytes()`) which are fixed-length on the happy path; a
  malformed-length attacker signature still runs the full max-length scan. Correct.
- **Timestamp-skew replay defense.** `(now_unix - ts).abs() > MAX_SKEW_SECS` (`events.rs:253`)
  rejects both stale (replay) and far-future timestamps, 5-minute window matching Slack's
  guidance. `now_unix` is injected, so this is deterministic and unit-tested
  (`tests.rs:957`). Correct.
- **Pinned to RFC/FIPS vectors.** `sha256_matches_fips_vectors` pins the empty + `"abc"`
  FIPS 180-4 digests; `hmac_sha256_matches_rfc4231_vector` pins RFC 4231 Test Case 2
  ("Jefe"/"what do ya want for nothing?"). Both are the canonical pins. The SHA-256 core
  (padding, K/H0 constants, compression) reads as a faithful FIPS 180-4 implementation.

**Concern (minor, defensive): SHA-256 length field assumes <2^61-byte inputs.** `bit_len`
is `(data.len() as u64).wrapping_mul(8)` (`hmac.rs:108`). For a webhook body this is never
remotely approached, and the input is already in memory (so it is `usize`-bounded), so this
is correct-in-practice. **Proposal:** none required for t25; if `hmac.rs` ever gets promoted
to a shared crypto leaf (see §3) and reused for large-object hashing (objstore's SigV4
already hashes payloads), add a debug-assert or a doc note on the input-length domain so the
shared version states its contract.

**Concern (minor): the unknown-top-level-type fall-through normalizes the envelope as an
event.** `parse_event` (`events.rs:222-226`) handles any `type` other than
`url_verification`/`event_callback` by `normalize(&envelope, envelope.get("event")
.unwrap_or(&envelope))`. This is *past* the signature gate, so it is not a trust-boundary
hole — but it manufactures a `SlackInbound::Event` from, e.g., a future
`app_rate_limited` envelope that has no `event` object, normalizing the whole envelope as if
it were the inner event. **Proposal:** return a distinct outcome (an `EventError` variant or
a `SlackInbound::Unhandled { ty }`) for an unrecognized top-level type rather than coercing
it into an `Event`, so the trigger bus can tell "a real event I don't model" from "a
non-event envelope". Low urgency — it is signature-verified and lossless (`raw` is retained)
— but it is a small translation-fidelity smell.

---

## 3. Duplicate crypto — reimplement vs share (the Lead's explicit question)

**Ruling: reimplementing was correct for t25; a shared `qfs-crypto-core` leaf is worth a
carry-over NOW, but extracting it inside t25 would have been premature.**

The reasoning the Constructor recorded (`hmac.rs:5-19`) is sound and I concur with it:
depending on objstore's *private* SHA-256/HMAC module would force a crate-dep on the
runtime-bearing `qfs-driver-objstore` — a leaf-confinement violation (and one the
`dep_direction.rs` generic leaf check would not even catch, since it guards `→ qfs-runtime`
edges, not driver→driver edges, but it would still be structurally wrong). Pulling a native
crypto crate (`ring`/openssl) breaks the wasm32 build that is the entire point of the pure
subset. And `sha2`/`hmac` are reported absent from the host cargo cache. So within t25's
boundary, a minimal pure reimplementation pinned to the same RFC vectors was the only move
that preserves both wasm-safety and leaf-confinement. Correct.

**But the duplication is now real and load-bearing in two drivers.** t22 objstore and t25
slack each carry a private, byte-identical-in-spirit SHA-256 + HMAC-SHA256. This is exactly
the drift hazard the team already paid down once — the t19 `qfs-http-core` extraction exists
*because* two hand-copied redaction sets had silently diverged and risked a token leak. A
second hand-copied *cryptographic* primitive is a strictly higher-stakes version of the same
smell: a divergence in the constant-time compare or the padding between two copies is a
security defect that no single-crate test catches.

**Proposal (carry-over, not a t25 revision):** open a carry-over to extract a
`qfs-crypto-core` **pure leaf** — `sha256`, `hmac_sha256`, `hex_lower`, `constant_time_eq` —
structurally a twin of `qfs-http-core` (no reqwest/tokio/runtime, depends on at most
`qfs-types` or nothing), and re-point both objstore and slack at it. This:
- collapses the duplication to one audited implementation (the FIPS/RFC vectors live in
  one place, the constant-time compare has one definition);
- stays wasm-safe (pure `core`/`alloc`, the property both drivers need);
- is admitted by the existing leaf-confinement test unchanged (a pure leaf is not a runtime
  consumer);
- and gives the wasm-subset precedent (§1) its natural crypto dependency.

I rule it **worth a carry-over now** (the second copy is the signal the team's own t19
precedent says to act on) but **correctly deferred out of t25** (extracting a shared crate
mid-ticket would have widened t25's blast radius and is a refactor that wants its own
review). Recommend the Lead file it as a near-term carry-over rather than a backlog item.

---

## 4. BodyErrorRule — false-success prevention, opt-in, already-done swallowing

**Confirmed on all three.**

- **Cannot surface a false success.** `BodyErrorRule::check` (`client.rs:183-208`) is invoked
  on **both** legs: `send_write` (`client.rs:341`) and `list` (`client.rs:365`). On `On`,
  `ok:false` → `SlackError::Body` (terminal). The write leg already gates on
  `resp.is_success()` first, then re-decodes the body and applies the rule — so an HTTP-200
  `{"ok":false}` is caught. The read leg applies the rule per page before merging. No path
  treats `ok:false` as success when the rule is on.
- **Opt-in, default-off per t18.** The enum default semantics are `Off` ignores `ok` entirely
  (`client.rs:189`). `SlackWsConfig::rule()` derives `On`/`Off` from `body_error_rule`, and
  `SlackWsConfig::new` sets it `true` *for Slack specifically* (`lib.rs:134`) with the doc
  noting the t18 default is off. The mechanism is opt-in; Slack opts in. Correct.
- **Already-done swallowed only for naturally-idempotent ops.** `is_already_done`
  (`client.rs:214`) is the closed set `{already_reacted, already_pinned, no_reaction,
  not_pinned}`. `swallow` is passed `true` *only* for `AddReaction`, `RemoveReaction`,
  `Pin`, `Unpin` (`client.rs:415,426,442,447` via `effect.swallows_already_done()` which is
  `AddReaction | Pin` — see concern below). `PostMessage`, `DeleteMessage`, `UpdateMessage`,
  file ops all pass `false`. So a `chat.delete` returning `message_not_found` is **not**
  swallowed — it surfaces. Correct discipline.

**Concern (minor): `swallows_already_done()` and the per-call `swallow` argument disagree on
`RemoveReaction`/`Unpin`.** `SlackEffect::swallows_already_done()` (`effect.rs:275`) returns
true only for `AddReaction | Pin`, but in `RestSlackClient::apply` the `RemoveReaction`
(`client.rs:426`) and `Unpin` (`client.rs:447`) calls pass `swallow` (the value of
`swallows_already_done()`, i.e. `false`) — so a `no_reaction`/`not_pinned` on a *remove* is
**not** swallowed even though `is_already_done` lists those codes as the symmetric
remove-already-landed class. The doc on `is_already_done` (`client.rs:210-212`) explicitly
says the remove-side codes *should* be no-op successes, but `swallows_already_done()` does
not include the remove variants, so they never are. This is an internal inconsistency: the
swallow-set predicate is narrower than the already-done code-set it feeds. It is not a
correctness *hazard* (the worst case is a remove of an already-absent reaction surfaces a
terminal `no_reaction` error instead of being a silent no-op — arguably the more
conservative behavior), but the code and its own doc are out of step. **Proposal:** either
widen `swallows_already_done()` to `AddReaction | RemoveReaction | Pin | Unpin` (matching
the documented intent that remove-already-done is idempotent too), or narrow
`is_already_done` + its doc to the add-only codes. Pick one so the predicate and the code
set are coherent. Recommend the former (remove *is* naturally idempotent — re-removing an
absent reaction is a no-op the user means to be one).

---

## 5. Pushdown residual truthfulness

**Confirmed — no silent row-drop.** `pushdown.rs` lowers only `ts`-boundary comparisons:
`ts >= / >` → `oldest`, `ts <= / <` → `latest` (`pushdown.rs:89-93`), and **only** a `ts`
column (`field != "ts"` returns `None`, `pushdown.rs:81`). Everything else — non-`ts`
columns, `OR`/`NOT`/`IN`/`BETWEEN`/`LIKE`, non-int/text literals — falls to the catch-all
that keeps the conjunct **residual** (`pushdown.rs:71` and the `None` arm at `:65-67`). An
`AND` recombines residuals correctly: a fully-pushed conjunct drops, a partial keeps the
unpushed side, both-residual rebuilds the `And`. `ReadPlan::list` (`read.rs:40-47`) further
restricts pushdown to message-log nodes (`Messages|Replies|Dms`); `users`/`files`/`reactions`
keep the **whole** predicate residual. The doc's invariant ("over-fetch then filter, never
wrong rows") is what the code does.

**One semantic caveat worth recording (not a defect): the `oldest`/`latest` inclusivity
assumption.** The module asserts Slack's `oldest` is inclusive and `latest` inclusive and
therefore drops the `ts`-boundary conjunct entirely. Slack's actual semantics are
`oldest`/`latest` *exclusive* by default (the `inclusive=true` param flips them), and the
strict `>`/`<` vs `>=`/`<=` distinction is collapsed (both `Gt` and `Ge` map to the same
`oldest`). If the real endpoint boundary does not match the assumed inclusivity, dropping the
residual would *over- or under-include the exact boundary `ts`*. **Proposal:** because this
boundary truthfulness can only be confirmed against the live API (parked for t38), keep the
residual for strict-vs-non-strict mismatches conservatively — i.e. when the comparison is
`Gt`/`Lt` but the pushed param is inclusive (or vice versa), push the param **and keep the
conjunct residual** rather than dropping it. That preserves "never wrong rows" without
needing the live API to be correct. This is a fidelity hardening, not a t25 blocker (the
ticket scopes richer pushdown to E3), but I'd flag it on the E3 carry-over so the
inclusivity is verified before the drop is trusted.

---

## 6. Token / secret safety

**Confirmed.** The bot token + signing secret are `CredentialKey` selectors in
`SlackWsConfig` (`lib.rs:112-114`) — never values — so the derived `Debug` is secret-free by
construction. The token is resolved only at request-build time in `RestSlackClient::request`
(`client.rs:252-264`), written into an `Authorization: Bearer …` header on a
`qfs_http_core::HttpRequest` whose redacting `Debug` hides it, and dropped. `SlackError` is
secret-free by construction (every variant carries op/status/code/shape, never a header
value — `error.rs`). Planted-canary coverage is present and real: `errors_are_secret_free`
(`tests.rs:720`), `rest_client_injects_bearer_token_and_never_logs_it` asserts the token is
absent from request `Debug` and `REDACTED` is present (`tests.rs:642-648`), and
`planted_token_and_signing_secret_never_appear_in_a_serialized_plan_or_config_debug`
(`tests.rs:748`) plants `xoxb-…` + a signing canary and asserts neither appears in a
serialized plan or a config `Debug`. Solid.

**Concern (minor): the signing-secret canary tests the config `Debug`, but `verify_signature`
takes `signing_secret: &str` as a bare string.** The signing secret crosses `parse_event`'s
boundary as a plain `&str`, not a `Secret` — correct for a pure wasm-safe function (a
`Secret` type would drag the secrets crate into the wasm subset), and the secret never enters
an `EventError` (all variants are `&'static str`/code, `events.rs:143-162`). So there is no
leak. But the canary suite does not include a test asserting the signing secret cannot appear
in an `EventError` `Display`/`Debug`. **Proposal:** add a one-line canary that runs
`verify_signature` with a planted signing secret against a tampered body and asserts the
secret string is absent from `format!("{err} {err:?}")` — symmetric with the bot-token
canary, closing the inbound trust boundary's secret-safety with the same evidence the
outbound side has.

---

## 7. Purity invariant

**Confirmed.** `#name`→`Cxxxx` resolution is documented and structured as applier/commit-time
I/O: the decoder keeps the **symbolic** channel (`effect.rs:153` etc. via
`ChannelRef::symbolic`), and `ChannelRef::is_id`/`symbolic` (`path.rs:46-61`) only *classify*
the token — they never perform a lookup. `client_msg_id` is deterministic and pure (derived
via HMAC over `node.id:channel:text`, no clock/RNG, `effect.rs:317-324`), so PREVIEW shows
the exact key COMMIT will send. `preview_of_a_pin_plan_surfaces_irreversible_and_performs_no_io`
(`tests.rs:690`) asserts PREVIEW records zero client calls. `parse_event` injects `now_unix`
for determinism (§2). The introspective surface (`describe`/`capabilities`/`pushdown`) builds
data with no I/O. Purity holds.

---

## 8. Multi-archetype coherence + parse-time capability gating

**Confirmed.** Per-node archetypes are coherent: messages/replies/reactions/dms = `AppendLog`,
files = `BlobNamespace`, users = `RelationalTable` (`schema.rs:16-24`), each with a typed
schema, snapshot-tested per archetype (`tests.rs:984`, `describe_emits_the_three_archetypes`).
Capability gating is node-keyed and rejects at parse time: `caps_for` (`lib.rs:202-217`)
narrows per `NodeKind`, an unparseable path → `Capabilities::none()`, and
`insert_and_update_on_users_are_rejected_at_parse_time_with_structured_error`
(`tests.rs:197`) confirms the gate fires with a structured error. `chat.postMessage`
non-idempotency is honored: a `client_msg_id` is attached (`effect.rs:154`), `is_at_least_once_post`
(`effect.rs:268`) is true only for `PostMessage`, and `retry_safe` (`applier.rs:75`) makes the
post the **only** effect a transient failure reports terminal rather than retryable
(`into_effect_error`, `applier.rs:100-119`) — so the runtime never auto-resends a post.
`reactions.add` is idempotent (swallowed). Irreversible flags are set on
`DeleteMessage`/`DeleteFile`/`Pin` (`effect.rs:254`) and on the `pin`/`delete` `ProcSig`s
(`procs.rs:60,84`). `INSERT INTO reactions` ≡ `CALL slack.react` equivalence is tested
(`tests.rs:462`). Coherent.

---

## 9. Dep direction / leaf confinement

**Confirmed.** The Slack crate is a clean runtime leaf: it depends on `qfs-runtime`
*optionally* and nothing depends back onto it. The allowlist append is present —
`"qfs-driver-slack"` is in `runtime_consumers_allowed` (`dep_direction.rs:333`) — and the
generic leaf check (`b`) plus the named-identity check (`b'`) both cover it without further
edit. The driver deliberately does **not** dep on `qfs-driver-http`, instead trading in the
shared `qfs-http-core` DTOs through a local `HttpTransport` seam (`client.rs:52`), which keeps
both the leaf-confinement and the t19 single-source-of-redaction-truth invariants intact. The
`SlackError::from(TransportError)` mapping (`error.rs:124-133`) is secret-free (only the
transport class crosses).

**Concern (trivial): `From<TransportError>` flattens the op label to `"http"`.** The `From`
impl hardcodes `op: "http"` (`error.rs:129`), so a transport failure loses which Slack call
it was (the `send_get`/`send_write` callers have `op` in scope but use `?`-via-`From` which
discards it). Purely an observability nit — the error is still correct and secret-free.
**Proposal:** if observability of *which* call timed out matters for the audit ledger, thread
`op` through (a small `map_err` at the call sites instead of `From`); otherwise leave it and
note that transport errors are op-agnostic by design.

---

## Cross-cutting coherence

The crate is internally consistent and faithfully translates the ticket: the path tree, the
archetype map, the verb→API map, the capability gate, the effect decoder, and the event
normalizer all agree on the same `NodeKind` taxonomy as the single pivot. The boundary
discipline (owned DTOs, no Slack JSON / reqwest type crossing) holds throughout. The wasm
precedent and the crypto-dedup question are the two structurally significant decisions and
both are sound — one to adopt, one to follow up on.

The minor concerns (the marker-only `events` feature wanting a mechanical fence, the
`swallows_already_done` vs `is_already_done` mismatch, the unknown-top-level-type
fall-through, the pushdown inclusivity assumption, the missing signing-secret canary, the
flattened transport op) are all non-blocking. None is a correctness or security hole at the
trust boundary; the signature verification — the one surface where a defect would be
critical — is correct.

**Decision: Approve with minor suggestions.** Recommend the Lead (a) adopt the wasm
feature-gating as the driver-layer convention and back it with a mechanical no-default-features
tokio-exclusion test, and (b) file the `qfs-crypto-core` shared-leaf extraction as a
near-term carry-over.
