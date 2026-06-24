# Architect Analytical Review — t35 POLICY engine (commit 025193b)

Reviewer: Architect (Neutral / structural bridge)
Scope: `crates/server/src/policy/{mod,model,enforce,gate,audit,grammar}.rs`,
`qfs-parser` POLICY grammar, `qfs-server::lower`, the three fire-path committers
(qfs-http / qfs-cron / qfs-watchtower), the `qfs` composition root (serve/cron/watchtower),
`EndpointDef`/`JobDef`/`TriggerDef` policy refs, and the fixtures.
Method: code/architectural review and model checking only — NO test/build/clippy execution.

## Decision: APPROVE WITH OBSERVATIONS

The gating boundary is the unavoidable choke point for every handler-fired write plan,
default-deny / fail-closed is sound, the ALLOW-ALL irreversible exclusion is correct and
tested both directions, atomic abort holds, and the audit emits exactly one secret-free
record per fire. Observations are documentation-drift and one defensible-but-loud-able
ergonomics call; none block acceptance.

---

## HEADLINE 1 — Is there ANY write/effect commit path that bypasses the gate?

**RULING: NO. The gate is the unavoidable choke point for every handler-fired write plan.**

Traced all three committers; in each, `evaluate`/`gate_plan` runs after `build_plan` and
strictly before the applier, and the apply site is the ONLY apply site:

- **qfs-cron** (`crates/cron/src/commit.rs`): `build_plan` (161) → `gate_plan` (177) →
  `record_fired` (179, unconditional, one record) → `if !is_allow() return PolicyDenied`
  (185) → applier constructed (197) and `qfs_core::commit` (199). The single apply is
  gated and after the deny return.
- **qfs-watchtower** (`crates/watchtower/src/commit.rs`): `build_plan` (198) →
  `gate_plan` (207) → `if Deny return PolicyDenied{...}` (209) → applier (225) +
  `qfs_core::commit` (226). Single apply, gated, after deny return.
- **qfs-http** (`crates/http/src/{policy,route,handler}.rs`): a write-lowering endpoint is
  gated at BOTH route compile (`route.rs:204 assert_read_only`) and request time
  (`handler.rs:133 assert_read_only`), each before `execute_read`. The HTTP path has no
  write applier at all — a write that even a granting policy admitted would still only reach
  `execute_read` (the read executor). Defence-in-depth is real here.

`grep` confirms the only `qfs_core::commit(`/`RecordingApplier` call sites in the three
fire-path crates are the two post-gate sites above. `verb_for_effect` (which maps an
Unknown kind to `None`) and `is_write_effect` have NO production gating callers — the
gating decision flows exclusively through `evaluate`/`classify_effect`, so the
Unknown-kind escape that a `None`-means-skip caller could create does not exist.

**Operator boot/shell vs. handler boundary — coherent and complete.** The operator
config-write channel (`qfs-server::runtime::boot` → `apply_source` → `lower_statement` →
`qfs_core::commit` over `ServerConfigApplier`, runtime.rs:166-192) and the t28 shell COMMIT
(`qfs_exec::run_oneshot`) are trusted operator actions and are correctly OUT of
handler-gating scope. Critically, the converse holds: **a TRIGGER/JOB whose DO body writes
`/server` config IS gated** — `build_plan` produces `EffectKind::ServerConfigWrite` nodes,
and `classify_effect` maps `ServerConfigWrite{op}` to its implied verb (enforce.rs:114-119),
so a handler that rewrites `/server` must be granted that verb. The boundary is therefore
"who fires it" (handler-committer path = gated; operator boot/shell = trusted), not "what it
targets" — which is exactly the coherent rule.

## HEADLINE 2 — Default-deny + read-path-vs-commit-plan model

**RULING: SOUND.** `evaluate` walks plan nodes; `Read`/`List` are `continue`-skipped
(enforce.rs:152), write verbs are gated, an unknown future `EffectKind` returns `Deny`
(enforce.rs:154-163). "Empty commit plan ⇒ Allow" is the right semantics: a pure SELECT
routes through the t29/t32 read path and `build_plan` returns `Plan::pure()` (exec.rs:150),
so `evaluate` sees no write node and Allow is vacuous — the read is NOT an ungated write
escape because the read path never reaches an applier. A no-policy / dangling-ref / empty
policy denies every write (`resolve_policy`, gate.rs:32-40; `Policy::default` is
default-DENY, model.rs:372-381). Fail-closed-on-unknown is genuine: `EffectKind` is
`#[non_exhaustive]` (plan/src/node.rs:21), so the `_ => EffectClass::Unknown` arm is
reachable for a future variant and the enforcer denies it (rule: None, reported as the
most-cautious CALL verb). t32 read endpoints still serve with no policy. Confirmed.

## HEADLINE 3 — ALLOW ALL excluding irreversible REMOVE/CALL

**RULING: RIGHT SAFETY SEMANTICS, acceptable as-is, with one ergonomics observation.**

A broad `ALLOW ALL` (the bare `ALL` token, `Rule.all_token = true`) does NOT grant
REMOVE/CALL; only an explicit verb list (`ALLOW REMOVE,CALL`) does (model.rs:339-354). The
hold-back applies ONLY to an `Allow` — a `DENY ALL` still denies the irreversible classes
(deny is never weakened). The `all_token` bit survives the `/server/policies` round-trip
(grammar.rs:116, tested in `roundtrip_all_token_and_glob`), so the strictness is durable,
not a parse-time-only artifact. Tested both directions:
`allow_all_token_does_not_grant_irreversible` and `explicit_verb_list_grants_irreversible`.
The fixtures exercise the real distinction — `leastpriv ALLOW INSERT,REMOVE` deliberately
lists REMOVE explicitly to grant the nightly JOB its irreversible delete.

This is defensible least-astonishment-for-safety (a blanket allow should not silently grant
an irreversible verb). **Observation (minor):** the surprise is real for an operator who
writes `ALLOW ALL` expecting "everything." The hold-back is documented in code
(model.rs:91-97, 133-139, 291-297) and in the fixtures, but there is no *runtime* signal —
a denied REMOVE under an `ALLOW ALL` policy surfaces only as a generic default-deny denial
("no rule matched"), which can read as a missing rule rather than the deliberate hold-back.
**Proposal:** when a denial occurs against a plan whose policy contains a broad-ALL allow
rule that WOULD have matched but for the irreversible hold-back, enrich the deny_reason
(e.g. `"REMOVE held back from broad ALLOW ALL; add an explicit ALLOW REMOVE"`). This is a
diagnostic refinement only — the *decision* is correct as-is; defer if the iteration budget
is tight.

---

## Other surfaces

1. **ALLOW/DENY/ALL as contextual idents — UNAMBIGUOUS.** Matched via `word("...")`
   (grammar.rs:205-210) which requires an UPPERCASE `Token::Ident`; `policy_rule_clause`
   runs ONLY inside `DdlKind::Policy` (grammar.rs:904). A column/path named lowercase
   `allow`/`deny` is an ordinary `Ident` and never matches `word("ALLOW")`. The frozen
   keyword golden stays at 38 (`keyword_count_is_frozen`, lang/keywords.rs:248-252) — no new
   frozen keyword, consistent with the t31 `AT` discipline. `POLICY`/`ON` remain the only
   frozen keywords used. Confirmed.

2. **Enforcer purity + correctness — CONFIRMED.** `evaluate` takes `&Policy, &Plan`, returns
   a `PolicyDecision`, performs no I/O and no mutation. Derives `(verb, driver, path)` from
   the node's carried `kind` + `target` (enforce.rs:146-148). Rules evaluated top-down,
   first match decides; no match falls to `policy.default`; returns the FIRST denial with
   node id + verb + driver + rule index (`first_denial_is_returned` test). Sound.

3. **can ∧ may layering — KEPT DISTINCT.** The t13 capability ("can") is the earlier
   resolve-time gate (exec.rs:195 maps a `Resolve` denial to a `Capability` exit class);
   policy ("may") is this layer with its own `PolicyError`/`PolicyDenied` error. A
   policy-denied plan is rejected even when the driver capability would permit. Legibly
   separate.

4. **Atomic abort — HOLDS.** In all three committers the deny path RETURNS before the
   applier is constructed; zero effects apply. `first_denial_is_returned` proves a
   mixed plan aborts at the first denied node (no partial cross-source run).

5. **Audit — exactly one record per fire, secret-free.** cron: one unconditional
   `record_fired(outcome.record(...))` before the deny return (commit.rs:178-184).
   watchtower: `record_allow` on Ok, `record_deny` on PolicyDenied — exactly one per fire
   (dispatch.rs:159-183). Effect summaries are `"<VERB> <driver>:<path>"` only
   (gate.rs:88-100); deny carries verb/driver/rule. `fired_record_is_secret_free_and_one_per_fire`
   asserts no payload column. Confirmed.

6. **Fixtures/tests are genuine default-deny, NOT weakened.** `server_boot.qfs`:
   `leastpriv ALLOW INSERT,REMOVE` (explicit REMOVE for the JOB), `trigger_writer`-style
   scoping; the read ENDPOINT carries NO policy (read path is separate). `watchtower.qfs`:
   `trigger_writer ALLOW UPSERT` grants exactly the idempotent UPSERT the handlers perform —
   no INSERT, no REMOVE/CALL. These are least-privilege grants, not an allow-all backdoor.

7. **Dangling policy ref → default-deny — CONFIRMED.** `resolve_policy(Some("ghost"), ..)`
   returns `Policy::new("ghost")` (no rules, default-DENY), not an error or allow
   (`dangling_ref_is_default_deny`).

8. **Round-trip — CONFIRMED.** `rfd_section8_golden_example` matches the RFD §8 DTO;
   `roundtrip_through_def` and `roundtrip_all_token_and_glob` round-trip CREATE ↔ PolicyDef ↔
   equal Policy; `malformed_rule_string_is_skipped` drops a garbage rule fail-closed (a
   malformed stored rule can never silently widen).

9. **Placement + confinement — CLEAN.** enforcer/model/gate/audit in `qfs-server::policy`;
   grammar tokens in `qfs-parser`; desugar in `qfs-server::lower::lower_policy`. No spine
   inversion (parser is a leaf; qfs-server depends on qfs-core). Re-exports are coherent
   (lib.rs:55-59).

---

## Observations (consolidated)

- **OBS-1 (doc drift, minor):** `crates/server/src/state.rs` doc comments still say POLICY is
  "enforced in t34" / "the full POLICY engine is t34" (lines 14-15, 58, 119). t35 now
  delivers the real enforcement; these comments are stale and should say t35. Also the
  watchtower `commit.rs` doc (line 53-59) and cron module docs reference t34 placeholders
  the t35 engine replaced — accurate as history but worth a one-line "superseded by t35"
  where it could mislead. Non-blocking. **Proposal:** sweep the `t34`→`t35` enforcement-era
  references in state.rs/commit.rs docstrings in the next iteration.

- **OBS-2 (ergonomics, see Headline 3):** broad-ALL irreversible hold-back lacks a runtime
  signal; a denied REMOVE under `ALLOW ALL` reads as a generic default-deny. **Proposal:**
  enrich the deny reason for the hold-back case. Diagnostic only; decision is correct.

- **OBS-3 (structural note, no action):** `ServerConfigWrite` is mapped to a write verb in
  `classify_effect`, so a handler writing `/server` is gated. Good. Worth a future test that
  a TRIGGER whose DO body writes `/server` is denied without an explicit grant — the model is
  correct; an explicit assertion would lock it against regression. Defer to t38/E2 runtime.

No concern rises to "Request revision." The security-critical invariants (no ungated write
path, default-deny, atomic abort, fail-closed unknown, secret-free single-record audit) all
hold.
