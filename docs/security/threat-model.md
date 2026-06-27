# qfs Security & Threat Model

> Ticket: t37 (security & threat model). Trip: `qfs-foundation-e0`.
> Implements RFD-0001 §10 (Security) and composes the controls built across the trip
> (§3 purity, §5 capabilities, §6 audit/idempotency, §8 server/policy). This document does
> **not** re-invent those mechanisms — it catalogs the defense-in-depth they form and maps
> **every attack-tree leaf to the concrete control that mitigates it AND the ticket that owns
> that control**.

## 1. System summary (why the blast radius is large)

`qfs` is **one binary** that holds long-lived tokens for Gmail, Drive, GitHub, Slack, AWS
(S3/SigV4), and Cloudflare (D1/KV/Queues), and runs cross-service effect-plans **unattended**
(scheduled JOBs, webhook-fired TRIGGERs, HTTP endpoints). A single compromise can therefore
read mail, delete drive files, push to GitHub, send Slack messages, and mutate cloud
infrastructure. The security posture is **defense-in-depth + fail-closed**: no single control
is trusted alone, and every gate defaults to refusal.

## 2. Assets

| # | Asset | Why it matters |
|---|-------|----------------|
| A1 | **Long-lived service tokens** (Gmail / Drive / GitHub / Slack / AWS / Cloudflare OAuth refresh tokens, API keys, SigV4 secret keys, webhook signing secrets) | Direct keys to every external service; exfiltration = full account takeover. |
| A2 | **The audit ledger** (`AuditLedger` on-disk fsync; `FiredPlanRecord` / `AuditEntry`) | The record of "what did this handler do?"; tampering/erasure hides an attack and breaks partial-failure recovery. |
| A3 | **User data in transit** (rows scanned from a source, effect payloads) | Mail bodies, drive contents, DB rows — sensitive content that must not leak into logs/error bodies. |
| A4 | **The `/server` config** (endpoints, jobs, triggers, webhooks, **policies**) | The control plane: rewriting a POLICY or adding an endpoint widens what handlers may do. |
| A5 | **Effect-plan integrity** (the `Plan` DAG before COMMIT) | A plan that touches a driver/verb it was never granted, or injects extra params, escalates privilege. |

## 3. Trust boundaries

| # | Boundary | What crosses | Trust change |
|---|----------|--------------|--------------|
| B1 | **CLI ↔ server** | argv / one-shot statements vs. the long-lived daemon | An interactive operator (a human present) vs. unattended execution (no human). The `RunMode` distinction (t37) lives here. |
| B2 | **server ↔ each driver** | a built `Plan`'s effect nodes; a driver-returned `CfsError` | The server trusts only the *typed* effect contract; a driver is treated as potentially buggy/compromised — its error text is NOT trusted into caller-facing bodies (t37 HTTP hygiene). |
| B3 | **inbound webhook ↔ fired plan** | a raw HTTP request (untrusted body + signature) → a TRIGGER's effect-plan | An anonymous internet sender becomes a plan author unless the signature is verified. |
| B4 | **operator-boot ↔ handler-fire** | a booted `/server` config (trusted author) vs. a plan a handler fires at runtime (must re-pass policy) | Boot-time config is operator-authored; a fired plan is re-authorized at COMMIT (TOCTOU defense). |
| B5 | **process memory ↔ any log/serialization sink** | a `Secret`'s bytes vs. a formatted span/event/JSON | Key material must never cross into a sink; the `Secret` type makes the crossing impossible by construction. |

## 4. Adversaries

| # | Adversary | Capability assumed |
|---|-----------|--------------------|
| Adv1 | **Compromised / buggy handler** | Authors a plan that tries to touch a driver/verb it should not, or emits a careless `CfsError` carrying upstream text. |
| Adv2 | **Malicious webhook sender** | Sends a forged inbound request to fire a TRIGGER without the signing secret. |
| Adv3 | **A leaked token** (already exfiltrated, or trying to exfiltrate) | Reads logs / error bodies / serialized plans hunting for a credential shape. |
| Adv4 | **A malicious config author** | Writes a `POLICY` / handler that over-grants, or relies on a broad `ALLOW ALL` to sneak in an irreversible verb. |
| Adv5 | **A path-traversal attempt** | Crafts a target path / param to escape the intended driver scope or shadow a typed parameter (injection). |

## 5. Control catalog (the defense-in-depth, by owning ticket)

| Control | What it enforces | Owning ticket(s) |
|---------|------------------|------------------|
| **`Secret<T>`** (`qfs_secrets::Secret`) | The only type holding key material: redacting `Debug`/`Display` (`Secret(***redacted***)`), **no** `Serialize`/`Deserialize`/`Clone`/`Deref`, `Zeroize` on drop, access only via `.expose()`. A secret cannot be *formatted or serialized* — the PRIMARY secret-out-of-logs control. | t27 (audited & reused as-is by t37) |
| **`LogScrubber`** (`qfs-cmd::redact` + `ScrubMakeWriter`) | Defense-in-depth backup: a scrub of every emitted log line (bearer tokens, `?sig=`/`X-…-Signature`, basic-auth userinfo) installed at the logging init so ALL sinks are scrubbed regardless of call site. Backup to `Secret`, not the primary. | t37 |
| **`qfs-crypto-core`** | Single-sourced SHA-256 / HMAC-SHA256 / `constant_time_eq` (the only crypto). | t34 |
| **POLICY default-deny + capability gate** (`qfs-server::policy::{evaluate, gate_plan, resolve_policy}`) | The `can ∧ may` authorize chokepoint: a handler with no/dangling policy denies every effect; a broad `ALLOW ALL` is held back from irreversible verbs. Wired into all three fire-path committers AFTER `build_plan`, BEFORE COMMIT (the TOCTOU re-check). | t35 |
| **Typed-param rewrite + param-shadow refusal** | A request param binds into a typed query placeholder; a param that would shadow a frozen/typed name is refused (injection defense). | t32 |
| **Webhook HMAC constant-time verify** | An inbound webhook's signature is verified with `constant_time_eq` before the TRIGGER's plan is built. | t34 |
| **Audit ledger** (`qfs-host::AuditLedger` on-disk fsync; `qfs-server` `FiredPlanRecord`/`AuditSink`) | Append-only, secret-free, fsync'd record of every fired plan + config mutation; the applied-effect ledger for recovery. | t36 |
| **`irreversible` flag + `IrreversibleGuard`/`RunMode`** (`qfs-core::security`) | Effect nodes carry `irreversible` (REMOVE/CALL). The guard fails closed in `Ci`/`Server`/`CliOneShot` without an explicit `--commit-irreversible` ack; interactive `Cli` prompts after PREVIEW. | t37 (flag: t35/E2) |
| **HTTP eval-error hygiene** (`qfs-http::error::eval_detail`) | Caller-facing error bodies render `code` + a generic per-class detail for non-allowlisted classes (Auth/Internal) instead of the raw `ExecError.message` — UNCONDITIONAL hygiene, not driver-dependent. | t37 (carry-over from t32) |
| **No-plaintext-token-`String` gate** (`qfs-secrets/tests/no_plaintext_token_string.rs`) | A mechanical CI test asserting no credential/plan type holds a secret-shaped `String` field — the type system, not discipline, keeps secrets out. | t37 |
| **Purity invariant** (`qfs-plan` is I/O-free; a `Plan` embeds only a connection *selector*, never a secret) | Nothing reaches I/O before the gates; PREVIEW-as-CI surfaces denials with no live creds. | t01/t10 (mechanically guarded), §3 |

## 6. Attack trees (every leaf → control + owning ticket)

### A1 — Long-lived service tokens

```
GOAL: exfiltrate or misuse a stored service token
├─ L1.1 token printed in a log line / {:?} dump
│     → Secret has redacting Debug/Display + no Serialize (PRIMARY);   [t27]
│       LogScrubber scrubs any shape that slipped past it (BACKUP).    [t37]
├─ L1.2 token serialized into JSON / an audit record / a config row
│     → Secret has NO Serialize/Deserialize (sealed serde); audit
│       records names+ops only, never payloads/credentials.           [t27, t36]
├─ L1.3 token left in freed memory after use
│     → Secret backs Zeroizing<Vec<u8>>; zeroed on drop.              [t27]
├─ L1.4 token leaked via a careless driver error into an HTTP body
│     → eval_detail() drops the raw message for Auth/Internal classes,
│       emitting code + generic detail (UNCONDITIONAL hygiene).        [t37 / t32]
├─ L1.5 token exfiltrated via a URL signature / basic-auth in a log
│     → LogScrubber scrubs ?sig=/&signature=/X-…-Signature and
│       scheme://user:pass@ userinfo.                                  [t37]
└─ L1.6 a NEW credential field added as a plaintext String (drift)
      → no-plaintext-token-String CI gate fails the build.            [t37]
```

### A2 — The audit ledger

```
GOAL: hide an attack by tampering with / evading the audit record
├─ L2.1 a fired plan executes without being recorded
│     → every committer emits exactly ONE FiredPlanRecord (allow AND
│       deny) via the gate seam before/after the fire.                 [t35, t36]
├─ L2.2 a crash mid-multi-leg cp loses the recovery trail
│     → commit()'s on_applied funnel writes one AppliedEffect per leg,
│       fsync'd to AuditLedger, so verify→delete is reconstructable.   [t36]
└─ L2.3 a credential/payload leaks INTO the ledger
      → AuditEntry/FiredPlanRecord are secret-free by construction
        (names + ops + verb/driver/rule only).                        [t36, t35]
```

### A3 — User data in transit

```
GOAL: read row contents / effect payloads not meant for the caller
├─ L3.1 a row payload echoed into an error body
│     → effect summaries are driver+path+verb only; ProblemBody is a
│       fixed owned shape, never an upstream payload.                  [t35, t32]
├─ L3.2 an upstream error string (possibly carrying data) returned verbatim
│     → eval_detail() reduces non-allowlisted classes to code+generic.  [t37]
└─ L3.3 a row payload logged
      → the gate/audit summaries never include payloads; LogScrubber
        is the additional net.                                        [t35, t37]
```

### A4 — The `/server` config (privilege via policy)

```
GOAL: widen what a handler may do
├─ L4.1 a handler with NO policy touches a driver/verb
│     → resolve_policy(None) ⇒ default-deny; evaluate denies every
│       write effect (fail closed).                                    [t35]
├─ L4.2 a dangling policy ref accidentally allows
│     → resolve_policy(Some(absent)) ⇒ default-deny policy.            [t35]
├─ L4.3 a broad `ALLOW ALL` sneaks in an irreversible REMOVE/CALL
│     → enforce holds irreversible verbs back from a broad ALL token;
│       OBS-2 deny reason names the held-back rule explicitly.         [t35 / t37 OBS-2]
└─ L4.4 a fired plan changes scope between boot-authorize and COMMIT
      → re-authorized at COMMIT with the same SecurityContext/gate
        (TOCTOU defense — boundary B4).                                [t35]
```

### A5 — Effect-plan integrity (injection / scope escape)

```
GOAL: make a plan touch more than the request intended
├─ L5.1 SQL/param injection through a request parameter
│     → typed-param rewrite binds values into typed placeholders;
│       no string interpolation of caller input into the query.       [t32]
├─ L5.2 a param shadows a typed/frozen name to alter the query
│     → param-shadow refusal rejects the bind.                        [t32]
├─ L5.3 a forged webhook fires a TRIGGER's plan
│     → HMAC constant-time signature verify before build_plan.        [t34]
├─ L5.4 an over-broad handler reaches an undeclared driver (SSRF-like)
│     → POLICY default-deny + the t13 capability (`can`) gate.         [t35, t13]
└─ L5.5 a path-traversal target escapes the driver scope
      → DriverGlob matches the LEADING path segments only; the policy
        reads the node's already-carried (driver, path), never derives
        from driver internals.                                        [t35]
```

## 7. Residual risk & assumptions

- **Redaction is best-effort; the real guarantee is `Secret` never being formatted.** The
  `LogScrubber` is explicitly defense-in-depth (it scans a conservative, documented shape set);
  an exotic secret shape logged as a raw `String` outside `Secret` could still slip past it. The
  load-bearing control is the type system (`Secret` + the no-plaintext-token-String gate).
- **The `IrreversibleGuard` governs the COMMIT seam, not reversibility of side effects already
  applied.** Once an ack is given and an irreversible leg applies, the audit ledger (A2) is the
  recovery/forensic source of truth — there is no automatic undo of a `mail.send`.
- **Driver-side secret hygiene is no longer trusted for caller-facing bodies** (t37 closed this),
  but a driver that logs its own secrets internally still depends on the `LogScrubber` backup;
  drivers should still route key material exclusively through `Secret`.
- **Offline-only validation.** All assurance (PREVIEW-as-CI, the no-creds golden, the audit
  reconstruct test) runs with no live credentials and offline stubs, so the controls are proven
  without ever touching a real token.

## 8. Carry-overs

- **t38/t39/t40**: when the real (non-recording) driver-backed committers land, wire the
  `IrreversibleGuard` server-side ack into the live commit path (today the serve fire paths run
  the gate with a fail-closed default; a future server-side `--commit-irreversible` equivalent
  should be surfaced through the daemon config). The CLI one-shot ack (`--commit-irreversible`) is
  fully wired.
- **t38+**: extend the `LogScrubber` shape set as new credential carriers appear (e.g. a new
  signed-URL scheme), and consider promoting the no-plaintext-token-String gate to also scan the
  driver crates once they declare credential structs.
