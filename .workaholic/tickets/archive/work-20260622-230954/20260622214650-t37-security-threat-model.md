---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Config, Infrastructure]
effort:
commit_hash: d784a7a
category: Added
depends_on: [20260622214650-t27-credential-secret-store-and-resolution.md, 20260622214650-t35-server-policy-access-control.md]
---

# Security & threat model

## Overview

`cfs` is one binary that holds long-lived tokens for Gmail, Drive, GitHub, Slack,
AWS, and Cloudflare and runs cross-service effect-plans unattended. That is a large
blast radius (RFD §10, §8 "Unattended-execution safety"). This ticket delivers the
**threat model document plus the cross-cutting enforcement code** that ties together
the defense-in-depth controls the RFD names: per-handler `POLICY` least privilege,
capability gating, an encrypted-never-logged credential store, dry-run (`PREVIEW`)
in CI, a full audit ledger, idempotent/recoverable effects, and explicit guards on
irreversible procs (`CALL mail.send`, deletes).

It implements RFD §10 (Security), and integrates the mechanisms specified in §3
(purity invariant — nothing executes until `COMMIT`), §5 (capabilities), §6
(audit ledger as applied-effect log; idempotency; recovery), and §8 (`CREATE POLICY`,
PREVIEW-as-CI). It does **not** re-invent those mechanisms; it composes them into one
auditable enforcement path and proves the invariants hold.

## Scope

In scope:
- A written threat model (`docs/security/threat-model.md`): assets, trust boundaries,
  adversaries, attack trees, and the control that mitigates each.
- A single `security` module that wires the chokepoints: a `SecurityContext` threaded
  through plan compilation and commit, enforcing policy + capability decisions.
- Secret-redaction guarantees: a `Secret<T>` wrapper whose `Debug`/`Display`/`serde`
  never emit plaintext, plus a log scrubber.
- `irreversible` effect gating: refuse to commit irreversible nodes without an explicit
  ack flag; always force `PREVIEW` for them in non-interactive/CI mode.
- Audit-ledger schema contract (the applied-effect ledger used for recovery) and the
  assertion that every committed effect is recorded.

Out of scope (deferred):
- The credential store implementation (encryption at rest, resolution) — t27.
- The `POLICY` grammar, parsing, and per-handler access-control evaluation — t35.
  This ticket *consumes* t35's decision API; it does not define policy syntax.
- Effect-plan/runtime mechanics (DAG, batching) — owned by E2 runtime tickets.

## Key components

New crate-internal module `security/` (consumer-side, no vendor types):

- `Secret<T>` (`security::secret`): newtype wrapping a credential/token.
  - `impl Debug`/`Display` print `Secret(****)`; `Serialize` errors or emits a tombstone;
    no `Deref` to the inner string — access only via `expose_secret(&self) -> &T`.
  - `Zeroize` on drop.
- `LogScrubber` (`security::redact`): a `tracing` layer that scrubs known secret shapes
  (bearer tokens, `?sig=`, basic-auth) from spans/events as defense-in-depth.
- `SecurityContext` (`security::ctx`): owned DTO carrying the resolved `PolicyDecision`
  source (from t35) and an `AuditSink`. Threaded into the interpreter.
  - `fn authorize(&self, effect: &Effect) -> Result<(), Denied>` — checks driver+verb
    against policy (t35) and the driver-declared `Capabilities` (RFD §5).
- `IrreversibleGuard` (`security::guard`): inspects `Plan` nodes' `irreversible` flag
  (RFD §6). `fn require_ack(plan: &Plan, mode: RunMode) -> Result<(), NeedsPreview>`;
  in `RunMode::Ci`/non-interactive, irreversible nodes hard-fail without an explicit
  `--commit-irreversible` ack.
- `AuditSink` trait (`security::audit`): `fn record(&self, entry: &AppliedEffect)`.
  `AppliedEffect` is an owned DTO (effect id, driver, verb, target path, `@version`,
  outcome, ts) — the applied-effect ledger from RFD §6 used for partial-failure recovery.
- Enforcement chokepoints (no new keywords — respects the closed-core grammar): policy +
  capability checks run at plan-compile (parse-time rejection per RFD §5) and again at
  `COMMIT` (TOCTOU defense); the purity invariant guarantees nothing reaches I/O before
  these gates (RFD §3).

## Implementation steps

1. Write `docs/security/threat-model.md`: enumerate assets (tokens, audit ledger, user
   data), trust boundaries (CLI↔server, server↔each driver, inbound webhook↔plan), and an
   attack tree per asset; map each leaf to its control and the ticket that owns it.
2. Add `Secret<T>` with `Zeroize`, custom `Debug`/`Display`, sealed serde; deny `Deref`.
   Replace any raw token/string fields in the credential-store API surface (t27) with it.
3. Add the `LogScrubber` `tracing` layer; install it in the logging init so all sinks are
   scrubbed regardless of call site.
4. Define `Effect`'s `irreversible: bool` consumption and implement `IrreversibleGuard`;
   wire `RunMode` (Cli interactive / Cli one-shot / Ci / Server) into the commit path.
5. Define `AuditSink` + `AppliedEffect`; provide a default append-only file/D1 sink;
   call `record` for every applied effect inside `COMMIT`, before and after each leg
   (so a crash mid-cp is reconstructable per RFD §6).
6. Build `SecurityContext::authorize`: combine t35 `PolicyDecision` with driver
   `Capabilities`; reject at plan-compile and re-check at commit.
7. Add CI harness: run every shipped handler/example plan through `PREVIEW` with no live
   creds (offline driver stubs) and assert zero effects execute.

## Considerations

- **Least privilege & secrets** (design): default-deny. A handler with no `POLICY` may
  touch nothing. Secrets live only inside `Secret<T>`; the type system, not discipline,
  is what keeps them out of logs/serialized plans. Audit the codebase for any `String`
  token via a clippy lint / grep gate in CI.
- **Idempotency & recovery** (operation, RFD §6): the audit ledger is the recovery source
  of truth — `cp` = copy→verify→delete, each step a ledger entry, so a partial cross-source
  plan can be replayed or rolled back. Prefer `UPSERT` for retry-safe at-least-once paths.
- **Observability**: structured audit entries must be queryable *through cfs itself*
  (`/server/audit` is a relation) so an operator can ask "what did this handler do?".
- **Hard parts**: (a) TOCTOU between parse-time capability rejection and commit-time policy
  — resolve by re-authorizing at commit with the same `SecurityContext`. (b) Redaction is
  best-effort; the real guarantee is `Secret<T>` never being formatted, so the scrubber is
  defense-in-depth, not the primary control. (c) Defining "irreversible" precisely — driver
  declares it per proc/verb; deletes and `CALL mail.send` are irreversible, `UPSERT` is not.
- **Directory/coding standards**: keep `security/` consumer-side with small traits; no
  vendor SDK types cross into it (owned DTOs only, RFD §9).

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; a custom lint /
  CI grep asserts no plaintext-token `String` fields on credential or plan types.
- Unit test: `format!("{:?}", Secret::new("abc"))` and serialized output contain no `abc`;
  `Zeroize` clears the buffer on drop.
- Plan assertion: a plan touching a driver/verb not granted by its `POLICY` is rejected at
  compile time with a structured error and never reaches `COMMIT`.
- Plan assertion: an irreversible-flagged plan in `RunMode::Ci` fails closed without the
  explicit ack flag; the same plan in interactive mode prompts after `PREVIEW`.
- Golden test: every shipped example handler runs through `PREVIEW` with offline stubs and
  **no live credentials**, producing the golden plan and zero applied effects.
- Audit test: committing a multi-leg `cp` writes one `AppliedEffect` per leg; killing the
  process after the copy leg leaves a ledger from which the verify→delete is reconstructable.
- `docs/security/threat-model.md` exists with one control mapped to every attack-tree leaf.
