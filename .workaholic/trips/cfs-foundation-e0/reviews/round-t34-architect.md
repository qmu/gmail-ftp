# Round t34 — Architect Analytical Review

Author: Architect (Neutral / structural bridge)
Phase: coding / review-and-testing
Reviewed: `fce7bdd` (cfs-crypto-core extraction), `695bfd3` (t34 watchtower)
Method: analytical / code + architectural review only (no test/build/clippy execution — disk park)

## Decision: Approve with observations

Both commits are structurally sound. The two headline regression-risk items —
the parser leading-`/` change and the TRIGGER `WHERE` wrapper — are **safe and
correct**, not regressions. The crypto-core extraction closes the ripe
carry-over correctly. The watchtower topology (option b + Fallback closure) is
the cleanest available resolution. Observations below are minor / carry-over
nature; none blocks acceptance.

---

## 1. cfs-crypto-core extraction — CONFIRMED done right

- **Algorithms unchanged + vector-pinned.** `sha256`/`hmac_sha256`/`hex_lower`/
  `constant_time_eq` are the same pure-std FIPS 180-4 / RFC 2104 routines the
  three copies carried. Pinned to FIPS 180-4 KATs (empty/`abc`/56-byte) and
  RFC 4231 TC2 in BOTH the in-crate `mod tests` AND a separate
  `tests/known_answer_vectors.rs` cross-crate vector pin. No drift.
- **No behavioral drift in the three consumers.** objstore `sigv4.rs` imports
  `{hex_lower, hmac_sha256, sha256_hex}`; cron `scheduler.rs`/`commit.rs` use
  `sha256_hex` (same run-id/fingerprint derivation, only the `use` reordered);
  slack `events.rs`/`effect.rs`/`tests.rs` route through `crate::hmac::*`. Call
  sites unchanged; the routine moved, the math did not.
- **Slack public `hmac::*` preserved.** `pub use cfs_crypto_core as hmac;` keeps
  the `cfs_driver_slack::hmac::{hmac_sha256, hex_lower, constant_time_eq}` path
  stable — no downstream import churn, the former private `src/hmac.rs` deleted.
- **Cron core stays pure (wasm).** `cfs-crypto-core` is a NON-optional cron dep
  (not behind a feature) and is itself a zero-dep std-only leaf, so the pure
  scheduler core that derives run-ids stays wasm-clean — no native-link hazard
  (the recorded reason for hand-rolling over `ring`/`sha2`).
- **Guard genuinely enforces the invariant.**
  `crypto_core_is_a_pure_leaf_single_sourcing_the_three_vendored_copies`
  asserts (a) `direct_deps["cfs-crypto-core"]` is **empty** (true pure leaf —
  stricter than the http-core guard, which permits cfs-secrets), and (b) all
  three former holders now depend on `cfs-crypto-core`. Both halves of the
  single-source + pure-leaf invariant are mechanically pinned.

## 2. Parser `raw_token_text` leading-`/` change — REGRESSION-SAFE (ruled)

**Ruling: correct fix, not a regression risk. No asymmetry. Must-fix NOT triggered.**

The scrutiny target is whether re-prepending the `/` to a `Token::Path` could
break the CO-t30-2/3 CREATE≡INSERT byte-equality or t32 route compilation.
Reasoning:

- **Scope is narrow and verified.** `raw_token_text` has exactly TWO callers:
  `on_clause` and `every_clause`. It is **NOT** the path-rendering primitive
  used by the binding desugar or the body-column parse — those go through
  `StatementSpec`/`PlanSpec` canonicalisation. So the change touches only the
  `on`/`every` operand text, never the DO body or the predicate spec.
- **The CREATE≡INSERT byte-equality is about the BODY spec, not `on`.** The
  `body_bearing_create_equals_its_insert_twin_via_canonical_spec` test compares
  the `plan`/DO body `PlanSpec` canonical — parsed identically on both sides.
  The leading-`/` change does not touch that path, so the equality is
  untouched. The `on` column is a raw string both ways: the CREATE form fills
  it from `raw_token_text`; the INSERT twin writes the column literally. A user
  writing the resolvable mount path (`/mail/inbox`) on the INSERT side now
  matches the CREATE form's stored `/mail/inbox` — they **shift together**, not
  asymmetrically.
- **ENDPOINT routes are unaffected.** ENDPOINT uses `ON 'GET /recent'` — a
  `Token::Str`, which goes through the unchanged `Token::Str(s) => Some(s)`
  arm. Only a BARE-path operand (`ON /mail/inbox`, a `Token::Path`) is
  affected. The t32 route compile (`split_method_route`) reads the quoted-string
  `on`, so its `Route` is unchanged. No route golden pins a bare-path `on`.
- **The change is required for downstream correctness.** `WatcherSet::from_state`
  classifies a poll-source trigger by `t.on.starts_with('/')`. A slash-less
  `mail/inbox` would silently fail to register as a watcher. The fix is the
  enabler for the watchtower's source-watcher classification — the design
  *depends* on the slash being present. This is a fix of a latent bug, not a
  regression.
- **Test coverage note (honest).** The 1061-pass suite exercises `ON inbox`
  (Ident) and `ON 'GET /recent'` (Str); there is no golden that pins a
  bare-`Token::Path` `on` body. So no golden breaks (none covers it), and the
  new behaviour is the *intended* canonical form. Observation O-1 proposes
  adding one.

## 3. Watchtower topology — option (b) + Fallback closure — CLEANEST resolution

**Ruling: cleanest available resolution; the Fallback closure is a clean
composition seam, not a coupling smell.**

- **No spine inversion / runtime-free / tokio dead-ends.** The
  `watchtower_binding_is_a_leaf_serve_consumer` guard pins (a) cfs-watchtower
  depends on cfs-server + cfs-exec, (b) only `cfs` consumes it (leaf), (c) NO
  cfs-runtime dep (the real commit path is the injected `Committer`), (d) NO
  cfs-http dep. tokio (LocalBus MPSC + dispatch loop) dead-ends in the binary —
  the t28 runtime-leaf exemption precondition holds. cfs-server stays
  runtime-free (the dispatcher never drives the COMMIT interpreter).
- **Fallback seam is clean.** `type Fallback = Arc<dyn Fn(&HttpRequest) ->
  Option<HttpResponse> + Send + Sync>`. The two leaves cross only through owned
  DTOs + one closure. `cfs-http` gains zero dependency on cfs-watchtower;
  `serve_config_full` threads the optional fallback, consulted on a router MISS
  before the 404 — matched routes always win, so the watchtower can never
  shadow a declared endpoint. The `Fn`/`Send`/`Sync` bound is correct for
  per-connection sharing.
- **Preferable to the alternatives.** Factoring shared cfs-http bits or
  extending the cfs-http consumer guard would either (i) force cfs-watchtower
  onto cfs-http (inverting the two-independent-leaves goal) or (ii) widen the
  cfs-http surface for one consumer. The closure is the minimal seam that keeps
  both leaves independent.
- **5-leaf exec-consumer generalization is principled.** The allowlist
  (`cfs-cmd, cfs, cfs-http, cfs-cron, cfs-watchtower`) admits cfs-watchtower for
  the same structural reason as cfs-cron/cfs-http: a LEAF integration consumer
  above the spine, consumed only by the terminal binary. The guard still fails a
  spine/lower crate reaching up into cfs-exec. This is a principled
  generalization of the established pattern, not allowlist creep — each entry is
  a binary-consumed serve leaf, and the rule (only the binary consumes it) is
  re-asserted per-leaf.
- **Trait identity.** `Vec<Box<dyn cfs_watchtower::Binding>>` passed to
  `serve_config_full` is sound: `cfs_watchtower::Binding` is a re-export of
  `cfs_server::Binding` (lib.rs:66), the same trait — no coercion needed.

## 4. TRIGGER `WHERE` representation — SOUND, acceptable as-is

**Ruling: the Query-over-empty-VALUES wrapper is a sound representation, not a
hack. Acceptable as-is.**

- **Unambiguously recoverable.** The grammar always emits exactly
  `Statement::Query(Pipeline { source: Values{empty}, ops: vec![Where(pred)] })`.
  `guard_matches` extracts `ops.first()` expecting `PipeOp::Where(e)` and errors
  (`NotAPredicate`, fail-closed) on any other shape. The empty VALUES source is
  a structural carrier only — never read. Round-trip through `StatementSpec`
  serde with no new AST node is the right call (it reuses the frozen-grammar
  serde surface; adding a node would be a larger grammar change).
- **`NEW.`-stripping does NOT collide with a real column.** Both the predicate
  resolver (`predicate.rs:resolve`) and the binder (`bind.rs`) match the
  **exact path segment** `"NEW"` (case-sensitive, uppercase) as the head of a
  multi-segment path — NOT a substring prefix. A column `new_foo` is the single
  segment `"new_foo"` (≠ `"NEW"`); a column literally named `new` accessed as a
  bare `new` (single segment, empty rest) falls through to direct resolution.
  Only `NEW.<col>` (two-segment, head exactly `NEW`) is stripped. No collision.
- **`where_pred` on `ServerDdl` is consistent with the t31 frozen-DDL design.**
  The `predicate` column is registered as a **body column** in
  `lower.rs:is_body_column` alongside `plan`/`query`, so the INSERT twin's
  `predicate` string is parsed into the same canonical `StatementSpec` the
  CREATE form builds — the CREATE≡INSERT byte-equality EXTENDS to the predicate
  (`trigger_where_guard_round_trips_into_the_predicate_spec`). `#[serde(default)]`
  on `TriggerDef.predicate`/`WebhookDef.secret` keeps pre-t34 rows
  round-tripping. The guard-less trigger emits no `predicate` column (stays
  byte-identical to a pre-t34 row). All consistent with the frozen design.

## Other surfaces — confirmations

1. **At-least-once + dedup — sound.** Ack only on `should_ack()` (every terminal
   `Dispatched`); a COMMIT failure returns `Err(FireError)` → no ack → the event
   stays in the LocalBus spool for `redeliver_unacked`. Dedup ledger checked
   FIRST (a redelivered key → `Duplicate`, no re-fire), advanced only after
   `fired > 0` (a partial commit failure redelivers + re-attempts). Spool/cursor/
   ledger in-memory parks are honestly documented (`watchtower.rs` module doc).
   `same_dedup_key_twice_yields_one_net_effect` proves one net effect + one
   audit across two deliveries via a counting committer.
2. **WHERE gating — sound.** `passes_guard` fail-closed on rehydrate/eval error;
   a gated event → `Dispatched::Gated`, zero fire, zero audit
   (`failing_where_guard_fires_zero_plans_and_zero_driver_calls`).
3. **Webhook signature — sound.** Per-webhook secret resolved BY HANDLE from
   cfs-secrets, exposed only inside `verify_signature`, never logged/in-DTO;
   HMAC via cfs-crypto-core + `constant_time_eq`; signed → 202 + 1 event,
   bad/missing sig → 401 + 0; empty handle = documented unsigned mode. Tests
   cover all three.
4. **Purity — sound.** Plan build + WHERE eval do no I/O / no mutation; the
   `building_the_plan_and_guard_perform_no_commit` test uses a PanicCommitter to
   prove a gated event never reaches the COMMIT boundary.
5. **Policy gate hook — real, not bypassed.** `PolicyGate::check` is called for
   EVERY matched, gated-in handler (dispatch.rs:143) before commit; a denial
   fires nothing. `AllowAllGate` is the documented t35 placeholder.
6. **Reconcile — sound.** `WatchtowerBinding::reconcile` is sync, rebuilds three
   atomic-swap snapshots (routes / watcher set / trigger set), holds the write
   guard only for the pointer swap, has no `.await`. Idempotent re-reconcile
   swaps an equal snapshot.
   (`reconcile_converges_webhook_routes_and_is_idempotent`).
7. **Parks honest.** PREVIEW-grade committer (RecordingApplier, same stage as
   cron/HTTP), watcher poll tasks not yet live-spawned (desired set exposed),
   raw body as one `body` field — all documented in the module docs, not
   overclaimed.

---

## Observations (each with a proposal)

- **O-1 (minor, carry-over-able): no golden pins a bare-`Token::Path` `on`.**
  The leading-`/` fix is correct but the suite covers only `ON inbox` /
  `ON 'GET /recent'`. **Proposal:** add a parser unit test asserting
  `ON /mail/inbox` yields `d.on == Some("/mail/inbox")` AND a
  `WatcherSet::from_state` test asserting such a trigger registers a watcher
  (closing the loop the fix exists to enable). Cheap, locks the canonical form.

- **O-2 (minor): a `Gated` redelivery is not collapsed by the ledger.** The
  dedup_key is recorded only on `fired > 0`; a redelivery of an all-handlers-
  gated event re-evaluates the guards. This is correct (no net effect either
  way) but means a guard re-runs. **Proposal:** document the intentional choice
  in the dispatch.rs at-least-once doc (it is implied but not stated) — or, if
  guard re-evaluation cost ever matters, record gated keys too. Acceptable as-is.

- **O-3 (minor): `BusError::Full` spools but is not auto-redelivered.** On a full
  channel `publish` spools the event but the live send fails; recovery relies on
  `redeliver_unacked`, which the binary does not auto-drive. Honestly a park.
  **Proposal:** note in the `watchtower.rs` parked-wiring list that the
  back-pressure redelivery driver is part of the durable-bus carry-over
  (currently the spool grows but is only drained on an explicit recovery call).

## Cross-cut coherence

The two commits compose coherently: crypto-core lands FIRST (fce7bdd) so the
watchtower webhook HMAC is the fourth CONSUMER of the single source rather than a
fourth copy. The watchtower reuses the established leaf-binding pattern
(cfs-cron/cfs-http), the injected-committer purity seam, the t31 frozen-DDL
body-column canonicalisation, and the t32 typed-substitution injection-safety —
no new structural idiom is introduced. Translation fidelity from RFD §8 (watch →
event bus → trigger → commit, at-least-once) to the implementation is faithful.
