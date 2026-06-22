# Coding E2E Review — t27 (Credential / secret store + multi-account resolution)

- Author: Planner (Progressive)
- Phase: Coding — E2E / external testing only (no code review)
- Target: t27 — `cfs-secrets` (`Secrets`, `Secret`, `InMemoryStore`, `EnvStore`, `resolve`, `grant_scopes`)
- Method: throwaway external-consumer crate in `/tmp/t27_e2e` (own `[workspace]`, path-deps on
  `crates/secrets` + `crates/types`; no production code; removed after the run). Built and ran with
  `cargo run`; the `Serialize` negative case run as two separate compile attempts.
- Status: **E2E approved**

## Verdict per item

| # | Item | Result |
|---|------|--------|
| 1 | Store round-trip + structured miss | PASS |
| 2 | Redaction (HARD) — canary absent everywhere + no `Serialize` | PASS |
| 3 | Multi-account resolution precedence | PASS |
| 4 | Cross-driver isolation | PASS |
| 5 | Scopes grant/deny | PASS |
| 6 | EnvStore via injected reader | PASS |

**Overall: E2E approved.** No redaction leak found; the hard redaction guarantee holds across every
text surface tested, and `Secret` has no serialization path (compile-enforced).

## Item 1 — Store round-trip (PASS)

- `put((mail, work), Secret::from(CANARY))` then `get` -> `expose_str()` and `expose()` both match
  the put value exactly.
- A `get` on a missing key returns `Err(SecretError)` with `code() == "secret_not_found"` — no panic,
  no value leak.
- The `NotFound` error text carries the *selectors* (`mail` / `work`) and never the credential value.
  - Display: `no credential for mail/nope`
  - Debug: `NotFound(CredentialKey { driver: DriverId("mail"), account: AccountId("nope") })`

## Item 2 — Redaction (HARD) (PASS — the headline guarantee)

Canary used: `SUPER-SECRET-CANARY-12345`. Confirmed the canary (and its `12345` fragment) appears
**NOWHERE**; only the redaction marker `***redacted***` renders.

Direct formatting of `Secret`:
- `{:?}` => `Secret(***redacted***)`
- `{}`   => `***redacted***`
- Nested in a `#[derive(Debug)]` holder: `github` renders, secret stays `***redacted***`, canary absent.

`Secret` does NOT implement `Serialize` — confirmed by **two hard compile errors** (no serialize path):
- Trait-bound assertion `assert_serialize::<T: serde::Serialize>(&secret)`:
  `error[E0277]: the trait bound `Secret: serde::Serialize` is not satisfied`
- Direct `serde_json::to_string(&secret)`:
  `error[E0277]: the trait bound `Secret: serde_core::ser::Serialize` is not satisfied`
  (compiler even suggests *adding* a derive — proof it is not implemented today).

Every error variant driven with the canary loaded into the store, then grepped for the canary —
all absent:
- `NotFound` Display: `no credential for mail/nope`
- `NotFound` Debug: `NotFound(CredentialKey { driver: DriverId("mail"), account: AccountId("nope") })`
- `ScopeError` Display: `account lacks required scope(s): mail.send`
- `ScopeError` Debug: `ScopeError { missing: ["mail.send"], held: ["mail.read"] }`
- `Ambiguous` Display: `ambiguous account for driver mail: candidates home, work — pass --account or AT 'acct'`
- `Ambiguous` Debug: `Ambiguous { driver: DriverId("mail"), candidates: [AccountId("home"), AccountId("work")] }`

No credential value (or fragment) surfaces in any of the above. **No leak -> no block.**

## Item 3 — Multi-account resolution (PASS)

Accounts `work` + `personal` registered for `mail`. Precedence outcomes:
- `--account work` selector -> `work`, `source = Flag` (explicit selector wins).
- `AT 'personal'` selector -> `personal`, `source = AtClause`.
- No selector + 2 accounts -> `ResolveError` `code == "account_ambiguous"` (structured, no silent pick).
- Typo'd selector `wrok` -> `ResolveError` `code == "account_unknown_selection"` (does not silently
  fall through to sole).
- Single account (`s3`/`prod`) + no selector -> `prod`, `source = Sole`.
- Persistent active (`active.set(mail, personal)`) with 2 accounts and no flag/AT -> `personal`,
  `source = Active` (active beats sole/ambiguity, below flag/AT).

## Item 4 — Cross-driver isolation (PASS)

- A credential under `(mail, work)` is reachable via `(mail, work)` but a `get` on `(slack, work)`
  returns `secret_not_found` — distinct keys by construction, no cross-driver reach.
- `list(Some(slack))` is empty: a driver only sees its own `(driver, account)` keys.

## Item 5 — Scopes (PASS)

- `grant_scopes(["mail.read"], ["mail.read","mail.send"])` -> `ScopeGrant { granted: ["mail.read"] }`
  (held superset of required grants).
- `grant_scopes(["mail.send"], ["mail.read"])` -> `ScopeError` `code == "scope_denied"`,
  `missing == ["mail.send"]` (structured, lists exactly what to re-consent for).
- `ScopeError` Display/Debug are secret-free (canary absent).

## Item 6 — EnvStore via injected reader (PASS)

Using `EnvStore::from_map("CFS_SECRET_", { CFS_SECRET_MAIL_WORK = CANARY })` — the injectable
fixture seam, NOT the real process env:
- `var_name((mail, work)) == "CFS_SECRET_MAIL_WORK"`.
- `get((mail, work))` resolves; `expose_str()` matches the injected value.
- `get((mail, missing))` -> structured `secret_not_found` miss (no panic, no value).

## Concern + proposal (Critical Review Policy)

- **Concern (business outcome / trace-ability):** the redaction guarantee is enforced today by the
  *absence* of `Serialize`/`Display`-of-value and by a CI `grep` for `.expose(` near `format!`/
  `tracing` (per the ticket). That guard is a convention, not a type-level wall; a future contributor
  could add a `Serialize` derive or a value-bearing `Backend(String)` from `.expose_str()` and the
  E2E suite here would still pass for the *current* code but the regression would ship. The blast
  radius (8 services in one binary) makes a silent regression here expensive.
- **Proposal:** add a compile-fail fixture to the crate's own test surface (e.g. a `trybuild`
  `tests/ui/secret_no_serialize.rs` asserting `Secret: Serialize` stays unsatisfied) so the
  no-serialize invariant is pinned by the crate's CI rather than only re-checked ad hoc by an
  external Planner run. This converts a convention into a regression gate and lets any stakeholder
  trace "secrets cannot be serialized" to a green test. This does **not** block approval — every
  redaction surface tested today is clean.
