# Architect Analytical Review — t37 (security & threat model)

- **Reviewer**: Architect (Neutral / structural bridge)
- **Commit**: `83cf65a` (1159 tests, clippy + fmt clean per Lead)
- **Ticket**: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t37-security-threat-model.md`
- **Mode**: analytical review only — no build/test/clippy (disk ~99%, green already confirmed).

## Decision: APPROVE WITH OBSERVATIONS

The threat-model doc is accurate against the wired code on every leaf I spot-checked; the
`IrreversibleGuard`/`RunMode` contract fails closed correctly and does **not** over-gate
reversible plans; the trailing-`COMMIT` keyword has **no** bypass; `Secret` reuse + the gate hold.
Two structural observations (one carry-over coverage boundary, one ordering nuance) and three minor
notes follow, none blocking.

---

## HEADLINE 1 — Threat-model doc completeness + honesty: ACCURATE

The doc is a genuine leaf→control map, and the controls it cites EXIST and are wired. Spot-checks:

- **L4.1 / L4.2 default-deny** — `evaluate()` (`enforce.rs:162`) falls a no-rule write to
  `policy.default` (`Deny` for the default/empty policy); `EffectClass::Unknown` for a future kind
  also fail-closes to `Deny`. `empty_policy_denies_every_effect` covers it. **Claim holds.**
- **L1.4 / L3.2 eval_detail drops the message for Auth/Internal** — `eval_detail()`
  (`error.rs:156`) returns `e.message.clone()` ONLY for Parse/Usage/Capability/CommitRequired/
  CommitFailed; Auth → `"{code}: a credential/authorization error occurred upstream"`, Internal →
  `"{code}: an internal error occurred"` — the raw message is dropped, UNCONDITIONALLY (not
  driver-gated). **Claim holds.**
- **L1.2 / L2.3 audit record is secret-free** — `AuditEntry` (`audit.rs`) variants carry only
  who/node/op/name/before-after/cause/`FiredPlanRecord` (handler+policy+verb/driver/rule+ts). No
  payload or credential field exists on the type. **Claim holds by construction.**
- **L4.3 OBS-2 broad-ALL hold-back** — `evaluate()` records `held_by_broad_all` and
  `deny_reason()` names the held-back rule; `allow_all_token_does_not_grant_irreversible` asserts
  it. **Claim holds.**

Honesty notes the doc gets RIGHT (not overstated): §7 states redaction is best-effort and the load
-bearing control is the type system; §8 openly parks the **server-side ack** as a t38/t39/t40
carry-over and states the serve fire paths run the gate with a fail-closed default today. The
ownership column distinguishes "owned by t35/t36/t27" from "t37" correctly (it does not claim t37
authored controls it only composes).

## HEADLINE 2 — IrreversibleGuard / RunMode contract: SOUND, NOT OVER-GATING, NO BYPASS

(a) **Fail-closed-by-default posture is right.** `require_ack` (`security.rs:120`): reversible ⇒
`Ok`; `Ack::Granted` ⇒ `Ok` in every mode; otherwise `Cli` ⇒ `Prompt`, all non-interactive
(`CliOneShot`/`Ci`/`Server`) ⇒ `Blocked`. The only interactive mode is `Cli`; the
prompt-after-PREVIEW is coherent (the gate returns `Prompt`, the caller drives the confirm — the
guard itself does no I/O, keeping it wasm-clean/pure). `CliOneShot` correctly treated like CI (no
TTY to confirm on).

(b) **Reversible plans are NOT over-gated — traced end to end.** `is_irreversible()`
(`plan.rs:109`) is `any(|n| n.irreversible)`. The flag is set by `EffectKind::is_inherently_
irreversible()` (`node.rs:59`) = `matches!(self, Remove)` ONLY; `Insert`/`Upsert`/`Update` are NOT
swept in, and `Call` irreversibility is per-node-declared (never auto). The `reversible_plan_passes
_in_every_mode` unit test and the `reversible_commit_needs_no_irreversible_ack` E2E (INSERT commits
with bare `--commit`) confirm. **No over-gating.**

(c) **No trailing-`COMMIT` keyword bypass.** In `run_oneshot_inner` (`exec/lib.rs`), `unwrap_plan`
folds a trailing `COMMIT` into the same `commit` bool (`commit || *c`), and the guard at line 193
runs on that bool regardless of which switch set it. The E2E
`trailing_commit_keyword_irreversible_also_fails_closed` asserts `COMMIT REMOVE …` fails closed at
exit 4 with `irreversible_ack_required`, parity with `--commit`. The guard is a property of the
commit seam, not the switch. **No escape hatch.**

(d) **Serve fire paths failing closed by default is ACCEPTABLE for E0.** Both `qfs-cron` and
`qfs-watchtower` committers call `require_ack(&plan, RunMode::Server, Ack::Absent)` after the t35
policy gate and before apply, returning `IrreversibleBlocked` on a block. There is no server-side
ack channel yet — this is a *conservative* gap (it refuses a legitimate scheduled REMOVE/CALL job,
it does not permit an illegitimate one), and it is **honestly documented** as the t38–t40 carry-over
in doc §8. For E0 (recording committers, no live drivers) this is the safe posture. See Observation 1.

## HEADLINE 3 — Secret reuse + no-plaintext-token gate: HOLDS

`Secret` is reused as-is (single type, `Zeroizing<Vec<u8>>`, no Clone/Serialize/Deserialize/Deref,
redacting Debug/Display, `expose`/`expose_str` the only doors). The Zeroize test proves the wipe at
the exact backing type under `forbid(unsafe)` (it cannot read freed memory, so it proves
`Zeroizing<Vec<u8>>::zeroize` zeroes a planted buffer — the same `Drop` `Secret` inherits — an
honest, sound proxy). The sealed-serde test proves the seal behaviorally via a `#[serde(skip)]`
parent whose JSON never contains the planted value, plus a compile-time `assert_serialize::<T>`
sanity. **What they claim is what they prove.**

The gate (`no_plaintext_token_string.rs`) scans `crates/secrets/src` + `crates/plan/src` for a
field whose NAME matches a secret fragment AND whose TYPE is bare `String`/`&str`/`Option<String>`
without `Secret`. `scanner_detects_a_planted_plaintext_token_field` proves the detector FIRES (so
green = "no violations", not "scanner broken"). It would catch a newly-added `pub access_token:
String` on a credential/plan type. The `Secret`-wrapped form is correctly exempt. **The gate
enforces "type system, not discipline".**

## Observations & proposals

**Observation 1 (structural, carry-over coverage — the serve ack channel).** The serve fire paths
hardcode `Ack::Absent`; there is no way for an operator to authorize a scheduled irreversible JOB
even when they legitimately want one (e.g. a nightly REMOVE of stale rows). This is correct
fail-closed for E0 but becomes a *functional* gap the moment live committers land.
*Proposal*: keep as-is for t37 (the doc §8 carry-over is accurate and the posture is safe), but the
t38–t40 ticket that wires the live committer MUST surface a per-JOB/per-TRIGGER
`commit_irreversible` flag in the `/server` config DTO threaded into `require_ack` — track it as an
explicit acceptance criterion there, not a soft "consider". This is a translation-fidelity item:
the CLI contract (ack exists) and the serve contract (ack does not exist) currently diverge, and the
divergence must close, not drift.

**Observation 2 (gate coverage boundary — driver credential structs).** The gate scans only
`secrets` + `plan`. A driver crate that later declares a `struct GithubCreds { token: String }`
would NOT be caught. The doc §8 names this as a carry-over ("promote the gate to scan driver crates
once they declare credential structs"). This is the right boundary for E0 (no driver credential
structs exist yet) but is a real coverage edge.
*Proposal*: accept for t37; when the first driver credential struct lands (E4/bootstrap), widen the
gate's scanned-dir list to include the driver crates in the SAME ticket — otherwise the "type
system keeps secrets out" guarantee silently stops covering the highest-risk surface (the crates
that actually hold tokens). Worth a one-line note on the t38+ driver tickets.

**Observation 3 (ordering nuance, non-blocking).** In both serve committers the irreversible gate
runs AFTER the policy gate, so a plan that is both policy-denied AND irreversible surfaces
`PolicyDenied`, never `IrreversibleBlocked`. This is the correct precedence (authorization before
acknowledgement) and matches the CLI (which has no policy layer), but it means an operator reading a
`PolicyDenied` does not learn the plan was *also* irreversible. Acceptable — fixing the policy
grant is the right first action regardless. No change requested; noted for fidelity.

**Minor note A (LogScrubber framing & wiring — correct).** Framed throughout (module docs,
`init_tracing` docs, doc §5/§7) as DEFENSE-IN-DEPTH backup to `Secret`, not the primary. Wired
globally: `init_tracing()` (called once at `run` entry, `lib.rs:156`) sets the single fmt
subscriber's writer to `ScrubMakeWriter`, so every emitted line from any crate/span passes through
`scrub` before the byte sink. `scrub_after_markers` advances the cursor past each replacement (no
re-match, terminates by construction). Covers bearer/basic/x-api-key/`?sig=`/`-signature=`/userinfo
shapes. Conservative-by-design (returns benign lines unchanged). Sound.

**Minor note B (eval class split — sound).** The message-passing set
(Parse/Usage/Capability/CommitRequired/CommitFailed) is the executor's own well-typed, secret-free
diagnostics; the dropped set (Auth/Internal) is exactly where upstream/driver text — the likely
carrier of a token — would land. No sensitive-bearing class is left in the message-passing set:
Capability is the federation's own "can't serve that verb" signal (driver/verb names only), not an
upstream string. `eval_status` independently maps Auth→403/Internal→500, consistent. Sound.

**Minor note C (OBS-2 leakage — none).** The enriched deny reason emits only `verb.label()`,
driver name, node id, and rule indices — all secret-free policy *coordinates*, the same shape the
plain default-deny already emitted. It names a near-match rule index, which is legitimate operator
guidance ("add an explicit ALLOW REMOVE"), not a policy-internals leak (it does not dump rule
bodies or other handlers' policies). Appropriate.

**Placement/confinement (correct).** `IrreversibleGuard`/`RunMode` in `qfs-core` (the shared
commit-seam hub both faces route through — pure data-in/out, wasm-clean, no I/O); `LogScrubber` in
`qfs-cmd` (the binary's logging init, the right home for a sink wrapper); the gate in
`qfs-secrets/tests` (a mechanical CI assertion). No spine inversion: the guard is pure over
`qfs-plan::Plan`, nothing in the pure spine depends back on the consumers. Confinement holds.

**TOCTOU (matches the wired re-check).** Doc B4/L4.4 claims re-authorization at COMMIT; both serve
committers run `gate_plan` over the freshly-`build_plan`'d plan against the live policy table
BEFORE apply (the t35 re-check), so the claim matches the wire. Holds.

## Net

The deliverable's security artifact (the threat-model doc) is accurate, not aspirational. The
security-critical contract change fails closed in the right places, refuses to over-gate reversible
work, and has no keyword bypass. The two carry-overs (serve-side ack channel, driver-crate gate
coverage) are honestly documented and are the right deferrals for E0, but each must become an
explicit acceptance criterion on the ticket that closes it — recorded here so the fidelity gap does
not drift. **Approve with observations.**
