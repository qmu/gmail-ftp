# Coding E2E Review (Planner) — t06 Name Resolution

- Author: Planner (Progressive)
- Phase/Step: coding / review-and-testing
- Target: t06 — Name resolution (CALL procedures + receiver-typed pure aliases)
- Method: external-consumer E2E. A throwaway crate OUTSIDE the qfs workspace
  (`/tmp/qfs_e2e_t06`, own `[workspace]`, path-deps on `crates/{core,parser,driver,types}`,
  no production code) implements `qfs_core::Driver` out-of-crate, registers fakes in a
  `MountRegistry`, parses real source with `qfs_parser::parse_statement`, and drives
  `qfs_core::Resolver::resolve_statement`, branching on every `ResolveError` arm.
- No-I/O probe: every fake driver's `applier()` returns a `PoisonApplier` whose `apply()`
  panics, and `describe()` panics. Any I/O-ish touch during resolution would panic; the
  whole run is also wrapped so an adversarial-statement panic is caught and reported.
- Result: **14 / 14 checks PASS. No panics. No I/O.**

## Verdict: E2E approved

## Per-check results

| # | Check | Verdict | Evidence |
|---|-------|---------|----------|
| 1 | `CALL mail.send(to => …)` resolves | PASS | `ResolvedCall{ driver: mail, proc: send, qualified: "mail.send", irreversible: true }` |
| 2 | `CALL mail.bogus()` → UnknownProcedure (branchable, available listed) | PASS | code `unknown_procedure`, `available: ["send"]` |
| 3 | `CALL nope.x()` → UnknownDriver | PASS | code `unknown_driver`, `driver: "nope"` |
| 4 | wrong arity → ArityMismatch | PASS | code `arity_mismatch`, `expected: 1, found: 2` |
| 5 | bad named arg → UnknownArg (params listed) | PASS | code `unknown_arg`, `arg: "bcc", params: ["to"]` |
| 6 | receiver-typed alias `… |> WHERE SEND()` over `/mail` desugars to `mail.send` | PASS | resolves to `mail.send`, `irreversible: true` |
| 7a | alias from non-providing receiver (single provider) → AliasNotProvided | PASS | `MERGE` over `/mail` → code `alias_not_provided` |
| 7b | alias two non-receiver drivers ship → AmbiguousAlias (both named) | PASS | `SEND` over `/git` receiver → `candidates: ["mail", "sms"]` |
| 7c | alias over value-less source → UnknownReceiver (fails closed) | PASS | `FROM VALUES (1) |> WHERE SEND()` → code `unknown_receiver` |
| 8 | namespace isolation: `git.merge` ≠ `github.merge` resolve DISTINCT | PASS | distinct `driver` AND distinct `qualified` |
| 9 | capability gate: `UPDATE /mail` (select+insert only) → UnsupportedVerb | PASS | `verb: "UPDATE", supported: ["SELECT","INSERT"]` |
| 9b | supported verb passes the gate (`INSERT INTO /mail`) | PASS | no error, no callables |
| 10 | 6 adversarial statements → no panic, structured outcome only | PASS | see Observation O1 |
| 11 | no I/O during resolution (poisoned `applier()`/`describe()` never fire) | PASS | full run completed without a single panic |

## Sample resolved output

```
CALL mail.send:        ResolvedCall { driver: DriverId("mail"), proc: "send", qualified: "mail.send", irreversible: true }
alias SEND desugar:    ResolvedCall { driver: DriverId("mail"), proc: "send", qualified: "mail.send", irreversible: true }
namespace isolation:   git=ResolvedCall { driver: DriverId("git"),    proc: "merge", qualified: "git.merge" }
                       github=ResolvedCall { driver: DriverId("github"), proc: "merge", qualified: "github.merge" }
```

## Structured ResolveError dumps (≥ 3 required; 8 captured, one per arm)

```
UnknownProcedure:  code=unknown_procedure  UnknownProcedure { driver: "mail", name: "bogus", available: ["send"] }
UnknownDriver:     code=unknown_driver     UnknownDriver { driver: "nope" }
ArityMismatch:     code=arity_mismatch     ArityMismatch { qualified: "mail.send", expected: 1, found: 2 }
UnknownArg:        code=unknown_arg        UnknownArg { qualified: "mail.send", arg: "bcc", params: ["to"] }
AliasNotProvided:  code=alias_not_provided AliasNotProvided { name: "MERGE", driver: "mail" }
AmbiguousAlias:    code=ambiguous_alias    AmbiguousAlias { name: "SEND", candidates: ["mail", "sms"] }
UnknownReceiver:   code=unknown_receiver   UnknownReceiver { name: "SEND" }
UnsupportedVerb:   code=unsupported_verb   UnsupportedVerb { path: "/mail/inbox", verb: "UPDATE", supported: ["SELECT", "INSERT"] }
```

Every arm is branchable by enum variant and by the stable `.code()` string. No arm
leaks credential/secret material (only proc/param/driver names and supported-verb labels).

## Critical-review observations (Planner domain: business outcome + stakeholder value)

- **O1 — Concern: `CALL` is namespace-keyed, independent of the `FROM` receiver path.**
  `FROM /github/repo |> CALL git.merge(...)` resolves to `git.merge` (NOT `github.merge`):
  the resolver keys `CALL` resolution on the explicit `driver.action` token, ignoring the
  upstream `FROM` path's driver. This is *correct and intended* per the ticket ("`CALL`
  resolves only procedures a driver declares … namespacing keeps `git.merge` ≠
  `github.merge`"; only **aliases** are receiver-typed). The business value — an AI/operator
  always gets the exact, explicitly-named procedure with zero implicit cross-routing — is
  preserved, and namespace isolation (check 8) still holds. Proposal: no code change; but
  for stakeholder traceability, a future federation/path-binding ticket (E3) should document
  that an explicit `CALL d.p` over a `/other/...` receiver is intentionally allowed, so a
  reviewer reading a plan does not mistake it for a routing bug. Logged as an outcome note,
  not a blocker.

- **O2 — Strength: fail-closed receiver typing is demonstrably safe.** All three "ambiguous /
  not-provided / unknown receiver" paths reject rather than guess (checks 7a/7b/7c), and the
  value-less `VALUES` source correctly yields `UnknownReceiver`. This is the load-bearing
  safety property for the alias ergonomics and it holds from the outside.

- **O3 — Strength: the public surface is genuinely external-implementable.** The fake drivers
  were written with zero access to any `qfs-*` internal item — only re-exported public types
  (`Driver`, `ProcSig`, `Param`, `AliasFn`, `Capabilities` builders, `MountRegistry`,
  `Resolver`, `ResolveError`). `Capabilities::none().select().insert()` and the `ProcSig`/
  `AliasFn` builders made the `#[non_exhaustive]` types ergonomic out-of-crate. This realises
  the "a new backend = zero keywords, owned DTOs only" business promise tangibly.

## Notes

- Throwaway crate removed after testing (no production code touched).
- Environment: `cargo 1.96.0`, run after `. "$HOME/.cargo/env"`.
- This is E2E / external validation only — no code review and no production code was written
  (per Planner Coding-Phase QA role).
