---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Config, Domain]
effort:
commit_hash: e73434f
category: Added
depends_on: [20260622214650-t31-server-binding-ddl.md, 20260622214650-t13-driver-contract-trait.md]
---

# Server: policy / access control (per-handler least privilege)

## Overview

Implements the unattended-execution safety controls from RFD §8 (Server) and §10
(Security): `CREATE POLICY` declares, per handler (endpoint / trigger / job /
webhook), the least-privilege set of drivers and verbs that handler's plan may
touch. A server holding tokens for Gmail+Drive+GitHub+Slack+AWS+CF running
cross-service plans is a large blast radius (§10); a policy bounds that radius so
a compromised or buggy handler cannot reach beyond its declared capability.

`CREATE POLICY` is frozen Server DDL (§3, §8) and is sugar over
`INSERT INTO /server/policies` — the server is a driver and its policies are
data. Enforcement happens at plan-construction/commit time: every plan a handler
fires is checked against its bound policy, and **every fired plan is audited**
(§6 audit ledger, §10). This ticket delivers the policy model, its grammar/DTO,
the capability-gating enforcer, and the audit hook — not the bindings themselves
(those land in t31) nor the credential store (t-auth/E5).

## Scope

In scope:
- `CREATE POLICY <name> ALLOW <verbs> [ON <driver-glob>] DENY <verbs> ...` parse
  into an owned `Policy` DTO; desugar to `INSERT INTO /server/policies`.
- A `PolicyDecision` enforcer that, given a `Plan` (typed effect DAG) and a bound
  `Policy`, returns Allow / Deny(reason) per effect node, default-deny.
- Attaching a policy to a binding (`... POLICY <name>` clause on endpoint/trigger/job).
- Verb taxonomy aligned to the closed core: `SELECT INSERT UPSERT UPDATE REMOVE CALL`.
- Driver-scope matching against the `/driver/...` path namespace (glob, e.g. `mail`, `s3/*`).
- Audit-ledger record for every fired plan: handler, policy, decision, effect summary.

Out of scope (deferred):
- Binding DDL itself (`CREATE ENDPOINT/TRIGGER/JOB`) — **t31** (depends_on).
- Driver capability declarations (which verbs a node *can* do) — **t13** (depends_on);
  policy is the *may* layer on top of t13's *can* layer.
- Credential/secret storage & redaction — **E5 auth** ticket.
- The persistent audit-log sink/storage backend — **E2 runtime** ticket; here we
  only emit the structured record.
- Per-row / data-level authorization (RLS) — future.

## Key components

New crate module `cfs-server/src/policy/`:

- `mod.rs` — re-exports; `Verb` enum mirroring closed-core effects:
  `enum Verb { Select, Insert, Upsert, Update, Remove, Call }`.
- `model.rs` — owned DTOs (no vendor leak):
  ```rust
  pub struct Policy {
      pub name: String,
      pub rules: Vec<Rule>,        // ordered; later rules refine earlier
      pub default: Effectivity,    // default-deny
  }
  pub struct Rule { pub effect: Effectivity, pub verbs: VerbSet, pub driver: DriverGlob }
  pub enum Effectivity { Allow, Deny }
  pub struct VerbSet(BitFlags<Verb>);   // ALL = every verb
  pub struct DriverGlob(String);        // matches first path segment(s) of /driver/...
  ```
- `grammar.rs` — winnow parser for the `CREATE POLICY` form; produces `Policy`;
  also the `POLICY <name>` attachment clause used by t31's binding grammar.
- `enforce.rs` — pure enforcement (purity invariant, §3):
  ```rust
  pub fn evaluate(policy: &Policy, plan: &Plan) -> PolicyDecision;
  pub enum PolicyDecision { Allow, Deny { node: EffectId, verb: Verb, driver: String, rule: Option<usize> } }
  ```
  Walks the effect DAG, derives `(Verb, driver)` from each effect node's target
  path + operation, evaluates rules top-down, default-deny on no match.
- `audit.rs` — `pub struct FiredPlanRecord { handler, policy, decision, effects, ts }`
  and `pub trait AuditSink { fn record(&self, r: FiredPlanRecord); }` (sink impl out of scope).
- Touches `cfs-server` binding registry to store a `policy: Option<String>` ref on
  each binding and resolve it at fire time; touches the interpreter entrypoint
  (`COMMIT`) so no plan from a handler runs unevaluated/unaudited.

## Implementation steps

1. Add `Verb`, `Effectivity`, `VerbSet` (bitflags) and `DriverGlob` in `policy/model.rs`.
2. Implement `Policy`/`Rule` DTOs + `Default` = default-deny, with serde (round-trips
   through `/server/policies` as rows).
3. Write the winnow grammar: `CREATE POLICY name (ALLOW|DENY) verbs [ON glob] ...`;
   `verbs` = comma list or `ALL`. Emit structured parse errors (AI-friendly, §5).
4. Desugar `CREATE POLICY` to `INSERT INTO /server/policies VALUES (...)`.
5. Implement `DriverGlob::matches(path: &Path)` against the leading `/driver/...` segment(s).
6. Implement `evaluate(policy, plan)`: derive `(Verb, driver)` per effect node, apply
   rules in order, default-deny; return first denial with the offending node + rule index.
7. Add `policy: Option<String>` to the binding model and resolve+evaluate it in the
   handler fire path before `COMMIT`; on Deny, abort the plan (no partial effects).
8. Emit a `FiredPlanRecord` to the `AuditSink` for **every** fired plan, allow or deny.
9. Add the `POLICY <name>` attachment clause hook consumed by t31's binding grammar.
10. Tests: grammar golden tests, enforcement plan-assertion tests, default-deny test,
    audit-emission test. Run `cargo build`, `cargo clippy -- -D warnings`, `cargo test`.

## Considerations

- **Least privilege & default-deny**: the safe default is *deny everything*; a policy
  only widens via `ALLOW`. A handler with no policy attached must be deny-all (fail
  closed), not allow-all. This is the single most important behavior to get right.
- **Capability vs. policy layering**: t13 says what a driver *can* do; policy says what
  a handler *may* do. Enforcement is `can ∧ may`. Keep the two checks distinct so
  errors are legible ("driver cannot REMOVE" vs "policy denies REMOVE on /mail").
- **Purity / effects-as-data (§3)**: `evaluate` is a pure function over the `Plan` DTO;
  it performs no I/O and never mutates the plan — it only classifies. The only impure
  step remains `COMMIT`, now guarded by the decision. This keeps PREVIEW-as-CI-test
  (§8, §10) able to surface policy denials with no live creds.
- **Irreversible effects (§6, §10)**: `CALL mail.send`, `REMOVE` are where policy earns
  its keep. The enforcer must treat the `irreversible` flag as a reason to be strict;
  consider requiring an explicit `ALLOW CALL` (never folded into a broad `ALLOW ALL`
  by accident — document this, decide in step 6).
- **Idempotency/recovery**: a denied plan must abort atomically before any effect node
  applies (no half-run cross-source plan); align with the cp=copy→verify→delete recovery
  model so a deny mid-DAG cannot strand state.
- **Observability/audit**: every fired plan (allow *and* deny) produces a structured
  record; deny records include the offending verb/driver/rule index. Never log secrets
  (§10) — record driver name + path, not credentials or payloads.
- **Owned DTOs**: `Policy`/`Rule` are cfs-owned; no vendor type leaks past the boundary.
- **Hard part**: deriving `(Verb, driver)` from heterogeneous effect nodes uniformly
  (a git commit INSERT vs an S3 UPSERT vs a `CALL`). Resolve by having the effect-plan
  node (E2) already carry `verb` + target `path`, so policy reads them rather than
  re-deriving from driver internals.

## Acceptance criteria

- `cargo build`, `cargo clippy -- -D warnings`, `cargo test` all green.
- Golden test: `CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL` parses
  to the expected `Policy` DTO (matches the RFD §8 example).
- Plan-assertion: a SELECT-only plan under that policy → `PolicyDecision::Allow`;
  a plan containing an `INSERT` or `CALL mail.send` → `Deny { verb, driver, rule }`.
- Default-deny: a handler with no policy, and an empty policy, both deny every effect.
- Layering: a plan denied by policy is rejected even when the driver capability (t13)
  would permit it, with a distinct error message.
- A denied plan applies **zero** effects (atomic abort assertion).
- Every evaluated plan (allow and deny) emits exactly one `FiredPlanRecord` to a test
  `AuditSink`; deny records carry the offending verb/driver/rule index; no secrets present.
- `CREATE POLICY` round-trips through `INSERT INTO /server/policies` and back to an
  equal `Policy`.
- All assertions run with no live credentials (pure enforcement over plan DTOs).
