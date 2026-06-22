# Coding-Phase E2E Review — t08 (Stdlib functions + driver-prelude mechanism)

- **Author**: Planner (Progressive)
- **Role**: Coding Phase QA — E2E / external-interface testing only (no code review, no unit tests)
- **Ticket**: `20260622214650-t08-stdlib-and-driver-preludes.md`
- **Method**: A throwaway external consumer crate (`/tmp/t08_e2e`, own `[workspace]`, path-deps
  on `crates/{core,parser,driver,types,plan}`, no production code) drove the **public**
  `cfs_core` surface only: `StdlibRegistry::{with_core, builtin, register_prelude,
  prelude_aliases, alias_providers}`, `BuiltinEval::{Scalar, Aggregate, TableValued}`,
  `AggregateFactory`, `EvalCtx::{pure, with_capabilities, with_last_run}`,
  `Evaluator::{with_stdlib, type_of_fn}`, and `Resolver::resolve_statement` over a
  `MountRegistry` with a fake mail/sms `Driver`. The external crate compiled cleanly
  (proves the re-export surface is self-contained) and ran to completion.
- **Result**: 46 PASS / 1 FAIL. The single FAIL is a genuine adversarial-input **panic**
  in production library code (see Check 5).

## Verdict: **E2E blocked — `FORMAT_DATE(i64::MAX)` panics on integer overflow (adversarial input)**

Checks 1–4 are fully approved. Check 5 (no panics on adversarial inputs) surfaced a real
defect that violates the ticket's own acceptance criterion ("no panics on adversarial
inputs", "lib code stays panic-free"). It is narrow (only the extreme upper i64 boundary),
so the fix is small, but it must land before E2E sign-off.

---

## PASS/FAIL per check

### Check 1 — Stdlib functions on real `Value` args (PASS)
All representative families confirmed through the public registry on real `cfs_types::Value`
arguments:

- **String**: `UPPER('café')='CAFÉ'`, `LOWER('AbC')='abc'`, `TRIM('  hi  ')='hi'`,
  `LENGTH('café')=4` (Unicode scalar count, not bytes), `SUBSTR('hello',2,3)='ell'`,
  `REPLACE('a.b.c','.','-')='a-b-c'`, `CONCAT('a',NULL,'b')='ab'`.
- **Number**: `ABS(-7)=7`, `ROUND(2.6)=3`, `FLOOR(2.9)=2`, `CEIL(2.1)=3`, `INT('42')=42`,
  `FLOAT('3.5')=3.5`.
- **Date round-trip**: `FORMAT_DATE(DATE('2026-06-22'))='2026-06-22'`;
  `DATE_ADD(d,10)` → `'2026-07-02'`; `DATE_DIFF(d+10,d)=10`.
- **Conditional**: `COALESCE(NULL,NULL,'x')='x'`, `IF(true,'a','b')='a'`,
  `IF(false,'a','b')='b'`.
- **Aggregate** over fixture column `[1, 2, 2, 3, NULL]`:
  `SUM=Float(8.0)`, `COUNT=Int(4)` (nulls skipped), `AVG=Float(2.0)`,
  `COUNT(DISTINCT)=Int(3)`.
- **Structured (not panic) errors**: `SUM` of text and `SUBSTR` out-of-domain both return
  structured errors (dumps below); `UPPER(123)` → `FnError::Type`.
- **Table-valued purity/gating**: `READ` with capabilities OFF → `CapabilityDenied`
  (no file I/O); with capabilities ON → deferred `PlanNodeKind::Read` node (still no I/O).

Sample aggregate result line (verbatim):
`SUM=Float(8.0)  COUNT=Int(4)  AVG=Float(2.0)  COUNT(DISTINCT)=Int(3)`

### Check 2 — Registry: unknown function → structured, branchable, deterministic (PASS)
- `Evaluator::type_of_fn("NO_SUCH_FN", 1, false)` → `Err(FnError::UnknownFunction { name:
  "NO_SUCH_FN" })`, `.code() == "unknown_function"` (AI-branchable).
- Two identical calls return identical results (deterministic classification).
- `SUM` outside an aggregate context → `FnError::AggregateOutsideAggregate` (a *typed*
  error, not a panic — the aggregate-vs-scalar dispatch rule holds).
- A real builtin still types: `UPPER/1` → `ColumnType::Text`.

### Check 3 — Determinism: frozen context (PASS)
- `NOW()` evaluated twice against the same `EvalCtx` returns the identical
  `Timestamp(1_750_000_000)`; `CURRENT_DATE()` twice returns the identical `Int(20_626)`.
- Re-evaluating with a *different* `EvalCtx` yields a different frozen value
  (`NOW() == Timestamp(999)`), proving the value is read from the injected context, never
  the wall clock — reproducible for the PREVIEW / AI / server story.
- `LAST_RUN()` is `Null` when unset and `Timestamp(555)` when injected (injected state,
  not ambient).

### Check 4 — Driver-prelude mechanism (PASS)
- **Register + namespacing**: `register_prelude(SEND -> "FROM /mail/drafts |> CALL
  mail.send")` succeeds; `prelude_aliases(mail)[0]` is `SEND` desugaring to `mail.send`.
- **Impure rejection**: a body with no `CALL` (`FROM /mail/drafts |> WHERE x = 1`) is
  rejected with `PreludeError::Impure` (`code == "prelude_impure_alias"`) — the purity
  invariant is enforced at registration (dump below).
- **No-flatten / namespacing**: the same alias `SEND` registered on both `mail` and `sms`
  stays scoped — `alias_providers("SEND") == ["mail", "sms"]` and the two desugar to
  `mail.send` / `sms.send` respectively (no global collision).
- **End-to-end t06 resolution (reuses t06)**: with a fake `/mail` driver mounted whose
  `prelude()` ships `SEND`, `Resolver::resolve_statement("FROM /mail/drafts |> WHERE
  SEND()")` resolves through the alias to:
  `ResolvedCall { driver: "mail", proc: "send", qualified: "mail.send", irreversible: true }`.
- **Receiver-typed scoping in resolution**: with both `/mail` and `/sms` mounted (both
  shipping `SEND`), `FROM /sms/outbox |> WHERE SEND()` scopes to `sms.send` — receiver
  typing disambiguates, no flatten, no global clash.

### Check 5 — No panics on adversarial inputs (**FAIL — one panic**)
21 pathological scalar inputs were driven through the public surface under
`catch_unwind`. Twenty are panic-free. **One panics:**

```
thread 'main' panicked at crates/core/src/stdlib/scalar.rs:546:13:
attempt to add with overflow
  PANIC on FORMAT_DATE([Int(9223372036854775807)])   // i64::MAX
```

Root cause: `civil_from_days(z)` (scalar.rs:545) begins with `let z = z + 719_468;`. For
`z == i64::MAX` this overflows. In a **debug** build it panics; in a **release** build the
same expression wraps silently and yields a wrong date — so this is both a panic *and* a
latent correctness bug. `FORMAT_DATE` is reachable from any user/AI plan that formats an
epoch-day `Int`, and `DATE_ADD` already saturates, so an in-band value can flow here.

Boundary, isolated with a probe (verbatim):
```
FORMAT_DATE(0)                    = Ok(Text("1970-01-01"))
FORMAT_DATE(20626)                = Ok(Text("2026-06-22"))
FORMAT_DATE(2932896)              = Ok(Text("9999-12-31"))
FORMAT_DATE(-719468)              = Ok(Text("0000-03-01"))
FORMAT_DATE(9223372036854775807)  = PANIC            // i64::MAX
FORMAT_DATE(-9223372036854775808) = Ok(Text("-25252734927764585-06-07"))  // i64::MIN: no panic, junk year
FORMAT_DATE(9223372036854056339)  = Ok(Text("25252734927766554-09-25"))   // i64::MAX - 719_468 boundary OK
```

Only inputs within ~719,468 of `i64::MAX` overflow; everything realistic is fine. A
secondary observation: `i64::MIN` does not panic but produces a nonsensical negative-year
string rather than a `Domain` error — a correctness oddity in the same function family.

Other Check-5 items all PASS:
- A denied `env('SECRET_TOKEN')` (capabilities off, value present in a `MapEnv`) returns
  `CapabilityDenied { builtin: "env", requested: "SECRET_TOKEN" }` — the error carries the
  **name** but **never the secret value** `hunter2…` (RFD §10, no-leak confirmed).
- A malformed alias body (`")(@#$ not cfs"`) → structured `PreludeError` (no panic).
- A duplicate alias within one prelude → `PreludeError::Duplicate`.
- An alias over a no-receiver source (`FROM VALUES (1) |> WHERE SEND()`) fails **closed**
  with a structured `ResolveError::UnknownReceiver` (no guess, no panic).

---

## Structured error dumps (verbatim)

**Error dump A — `SUM` of text (type error, not panic):**
```
Err(Type { name: "SUM", expected: "Float", found: "Text" })
```

**Error dump B — `SUBSTR('hello', 0)` out-of-domain (domain error, not panic):**
```
Err(Domain { name: "SUBSTR", reason: "start_must_be_one_based" })
```

**Prelude-alias resolution result (the desugaring, end-to-end through t06):**
```
resolve("FROM /mail/drafts |> WHERE SEND()") =
  Ok([ResolvedCall { driver: DriverId("mail"), proc: "send",
                     qualified: "mail.send", irreversible: true }])
```

**Impure-alias rejection (purity invariant at registration):**
```
register_prelude("FROM /mail/drafts |> WHERE x = 1") =
  Err(Impure { driver: "mail", name: "BAD" })   // code = "prelude_impure_alias"
```

---

## Business-outcome assessment (Planner domain)

t08 delivers the stable, AI-facing vocabulary the product story depends on: the AI gets a
small fixed function set, **deterministic** `NOW`/`CURRENT_DATE` (so PREVIEW and golden
runs are reproducible — directly load-bearing for the unattended/server story), structured
machine-branchable errors (no prose parsing), and driver preludes that stay receiver-scoped
so two drivers can both ship `SEND` without collision. All of that is confirmed working
from the outside. The value proposition is intact.

The one concern is the `FORMAT_DATE` overflow panic. Trade-off framed in business terms: an
unattended server or an AI-authored plan that formats a computed epoch-day value could hit
an extreme `Int` and either crash the handler (debug) or emit a silently-wrong date
(release) — both undermine the "reproducible, never-surprises" promise the determinism work
is selling. It is a narrow boundary, so it does not threaten the architecture, only the
"panic-free / adversarial-safe" acceptance bar.

**Constructive proposal (concrete, business-framed):** make `FORMAT_DATE` total over the
whole `i64` domain. Two options, either acceptable:
1. **Clamp + domain error** — reject epoch-days outside the representable proleptic-Gregorian
   range with `FnError::Domain { name: "FORMAT_DATE", reason: "date_out_of_range" }`
   (mirrors `PARSE_DATE`'s existing domain-error shape; the AI already knows how to branch
   on `fn_domain`). This is the most consistent with the rest of the family.
2. **Saturating arithmetic** in `civil_from_days` (`checked_add`/`saturating_add` on the
   `z + 719_468` and the `era * …` / `365 * yoe` products), returning a clamped boundary
   date. Cheaper, but yields a far-future/past string rather than a clean error.

Option 1 is preferred: it keeps the "structured error, never a panic, never junk" contract
uniform across the date family (and would also fix the `i64::MIN` junk-year oddity). A unit
test pinning `FORMAT_DATE(i64::MAX)` and `FORMAT_DATE(i64::MIN)` to a structured error
should accompany the fix.

## Iteration request

Return to Constructor for the `FORMAT_DATE` overflow fix (Constructor implements + internal
test; Architect re-reviews; Planner re-runs this same external harness to confirm Check 5
reaches 0 panics). All other checks are approved and need not be re-litigated.
