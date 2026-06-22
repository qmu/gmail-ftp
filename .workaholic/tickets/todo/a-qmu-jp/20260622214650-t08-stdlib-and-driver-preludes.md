---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260622214650-t06-name-resolution-call-and-aliases.md]
---

# Standard library functions + driver-prelude mechanism

## Overview
This ticket ships the cfs **standard library** of built-in functions and the
**driver-prelude registration mechanism** that lets a driver contribute pure alias
functions (e.g. `fn SEND(d) = d |> CALL mail.send`) next to its procedures. It realizes
RFD §3 ("closed core + three open registries" — specifically the *functions/procedures*
registry and the "ergonomic aliases are pure functions, never keywords" rule), the
**purity invariant** (§3: every function has type `… -> Plan`/value-in-pure-context and
constructs, never performs), and the driver contract's **Prelude** declaration (§5).
It builds directly on t06's name resolution: t06 *resolves* identifiers against the
registry; this ticket *populates* that registry with the core stdlib and wires the
per-driver prelude into it, plus defines the evaluation contract for the pure functions.

The stdlib is the small, stable vocabulary the AI relies on in every plan — scalar
string/date/number helpers, `READ`/`NOW`/`CURRENT_DATE`/`LAST_RUN` context functions,
aggregates, `env()`, and the ad-hoc table-valued `http.get(...)` — so a single grammar
covers data shaping without per-driver verbs.

## Scope
In scope:
- Core **scalar** fns: string (`UPPER LOWER TRIM SUBSTR LENGTH SPLIT REPLACE CONCAT LIKE`-helpers),
  `BASENAME`/`DIRNAME`/`EXT` (path), date (`DATE PARSE_DATE FORMAT_DATE DATE_ADD DATE_DIFF`),
  number (`ABS ROUND FLOOR CEIL`, casts), `COALESCE`, `IF`.
- **Context** fns: `NOW()`, `CURRENT_DATE()`, `LAST_RUN()` (job state, RFD §8), `READ(path)`
  (blob read → bytes/text, pairs with codecs), `env(name)`.
- **Aggregate** fns over groups: `COUNT SUM AVG MIN MAX` (+ `COUNT(DISTINCT …)`), usable
  under `AGGREGATE … GROUP BY` (grammar from t04).
- **Table-valued** `http.get(url, headers=>…)` — ad-hoc REST as a row source (returns a
  one-row `{status, headers, body}` relation; body decoded via codecs downstream).
- The **prelude mechanism**: a `Prelude` DTO a driver returns, merged into the function
  registry namespaced/receiver-scoped so t06 can resolve it; alias bodies are parsed
  cfs (`d |> CALL …`) so they obey the purity invariant by construction.
- A `StdlibRegistry` populating t06's `CallRegistry::core_fns()` + `prelude_aliases()`.

Out of scope (deferred):
- Identifier **resolution** of these fns (already t06; this ticket only *registers* and
  *evaluates*).
- Actual effect execution / `Plan` DAG application → **E2 effect-plan & runtime** (this
  ticket only constructs plan/value nodes; `http.get`/`READ` define their *effect node*,
  not its execution).
- Codec bodies (`DECODE`/`ENCODE` json/yaml/…) → **codec-registry ticket** (we call into
  the codec trait, do not implement formats here).
- Pushdown of aggregates into a source DB → **E3 federation**.
- Driver path/capability wiring & real driver impls → **E4 drivers** (we ship a test mail
  driver prelude only).

## Key components
New module `crates/lang/src/stdlib/` (crate `cfs-stdlib`), depending on `cfs-ast` (t04),
`cfs-types` (t05), and `cfs-resolve` (t06). Pure, no vendor types — drivers contribute via
owned DTOs only.

```rust
/// A pure built-in. Evaluates in a pure context to a Value, or constructs a Plan node
/// for the effectful-but-deferred ones (READ, http.get). Never performs I/O itself.
pub struct BuiltinFn {
    pub name: Ident,
    pub sig: FnSig,                 // from t05; arg types + return Type
    pub eval: BuiltinEval,          // Scalar | Aggregate | TableValued
}

pub enum BuiltinEval {
    Scalar(fn(&[Value], &EvalCtx) -> Result<Value, EvalError>),
    Aggregate(Box<dyn AggregateFactory>),     // init/accumulate/finalize
    TableValued(fn(&[Value]) -> Result<PlanNode, EvalError>), // http.get, READ
}

/// Read-only context the pure fns may consult (NOW/LAST_RUN/env are data, not I/O here).
pub struct EvalCtx<'a> {
    pub now: DateTime,             // frozen per-statement (determinism)
    pub current_date: Date,
    pub last_run: Option<DateTime>,// injected by server/job binding (RFD §8)
    pub env: &'a dyn EnvSource,    // capability-gated; see Considerations
}

/// What a driver ships alongside its procs (RFD §5 Prelude). Bodies are cfs source.
pub struct Prelude {
    pub driver: DriverId,
    pub aliases: Vec<AliasDecl>,   // e.g. AliasDecl{ name:"SEND", body:"d |> CALL mail.send" }
}

pub trait HasPrelude { fn prelude(&self) -> Prelude; }   // implemented by Driver (E4)

/// Registers core stdlib + merges all driver preludes; exposes t06's CallRegistry surface.
pub struct StdlibRegistry { core: Vec<BuiltinFn>, preludes: Vec<Prelude> }
impl StdlibRegistry {
    pub fn with_core() -> Self;                          // all built-ins above
    pub fn register_prelude(&mut self, p: Prelude) -> Result<(), PreludeError>;
    pub fn parse_alias_bodies(&self) -> Result<Vec<AliasFn>, PreludeError>; // parse+purity check
}

pub enum PreludeError {
    AliasParse { driver: DriverId, name: Ident, source: ParseError },
    ImpureAlias { driver: DriverId, name: Ident },      // body not `-> Plan`
    DuplicateAlias { driver: DriverId, name: Ident },
}
```

Touches: `cfs-resolve` (`StdlibRegistry` implements `CallRegistry`), `cfs-types`
(`FnSig` instances for each builtin), `cfs-ast` (re-uses pipe/CALL nodes to parse alias
bodies). Aggregates plug into the `AGGREGATE` AST node from t04.

## Implementation steps
1. Define `BuiltinFn`, `BuiltinEval`, `EvalCtx`, `EnvSource`, `EvalError`.
2. Implement scalar fns (string/path/date/number/`COALESCE`/`IF`) with `FnSig`s registered
   in `cfs-types`; cover `BASENAME`/`SUBSTR` edge cases (unicode, empty, out-of-range).
3. Implement context fns: `NOW`/`CURRENT_DATE` read from `EvalCtx` (frozen per statement),
   `LAST_RUN` from injected job state, `env(name)` via `EnvSource`.
4. Implement aggregates via `AggregateFactory` (init/accumulate/finalize), wired to the
   `AGGREGATE … GROUP BY` evaluator; `COUNT(DISTINCT)` variant.
5. Implement `READ(path)` and `http.get(url, …)` as `TableValued`/plan-constructing nodes
   that emit a deferred effect/source node (no network here) — they return a `PlanNode`,
   honoring the purity invariant.
6. Build `StdlibRegistry::with_core()`; implement `CallRegistry` for it so t06 resolves
   against it unchanged.
7. Define the `Prelude`/`HasPrelude`/`AliasDecl` DTOs; implement `register_prelude` +
   `parse_alias_bodies` (parse each body as cfs, assert `-> Plan`, detect duplicates).
8. Seed a test mail driver prelude (`SEND`(mail)) and assert it round-trips through t06's
   receiver-typed resolution (`mail-drafts |> SEND` desugars to `… |> CALL mail.send`).
9. Wire `env()` and `READ`/`http.get` behind a capability/policy gate flag (default off in
   pure-eval tests) so unattended execution can deny them (RFD §10).

## Considerations
- **Purity invariant is load-bearing (RFD §3).** Scalar/aggregate fns return `Value`;
  the effectful-shaped ones (`READ`, `http.get`) return a *plan/source node*, never doing
  I/O in `stdlib`. Enforce at registration: an alias body that does not type to `-> Plan`
  is rejected (`ImpureAlias`). This keeps every plan dry-runnable.
- **Determinism / observability.** `NOW`/`CURRENT_DATE` are *frozen per statement* in
  `EvalCtx`; never call the wall clock mid-evaluation — this makes PREVIEW reproducible and
  golden tests stable. `LAST_RUN` is injected state, not ambient.
- **Least-privilege & secrets (RFD §10).** `env()`, `READ`, and `http.get` reach outside
  the pure data plane; gate them through a capability/`POLICY` check and never log secret
  values an `env()` may return. `env()` resolves through `EnvSource`, which the server can
  restrict per-handler; default-deny in unattended contexts.
- **Idempotency/recovery.** `http.get`/`READ` produce read source nodes (safe to retry);
  any write-shaped alias desugars to a `CALL` whose `irreversible` flag is set downstream
  (E2) — preludes must not hide irreversibility.
- **Hard part: receiver-typed prelude scoping.** Aliases must enter scope *only* for plans
  whose receiver driver shipped them; we register them namespaced by `DriverId` and rely on
  t06's receiver resolution — `StdlibRegistry` must not flatten preludes into the global
  core namespace (would break collision-proofing and ambiguity rules). Duplicate alias
  names across drivers are fine (scoped); duplicates *within* one prelude are `DuplicateAlias`.
- **Hard part: aggregate vs scalar dispatch.** `COUNT`/`SUM` are only valid under
  `AGGREGATE`; resolution must distinguish context. Encode this in `FnSig`/`BuiltinEval`
  so misuse (e.g. `SUM` in a `WHERE`) is a typed error, not a runtime panic.
- **Directory/standards.** Keep `stdlib` pure and I/O-free; one module per fn family;
  unit-testable in isolation; no driver network calls. Owned DTOs only across the boundary.

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; no live creds.
- Scalar golden tests: `BASENAME('/a/b/c.txt')='c.txt'`, `SUBSTR` unicode/bounds,
  date round-trips (`FORMAT_DATE(PARSE_DATE(...))`), `COALESCE`/`IF` semantics.
- `NOW()`/`CURRENT_DATE()` return the frozen `EvalCtx` value (determinism test: two calls
  in one statement are equal); `LAST_RUN()` returns the injected value / `NULL` when unset.
- Aggregates: `COUNT/SUM/AVG/MIN/MAX` and `COUNT(DISTINCT)` over a fixture relation match
  expected results; `SUM` used outside `AGGREGATE` is a typed error (not a panic).
- `http.get(...)` and `READ(...)` evaluate to a **deferred plan/source node** (assert node
  shape) and perform **no network/file I/O** during evaluation (no-live-creds, sandboxed).
- `env('X')` resolves via a stub `EnvSource`; with the capability gate off it is denied;
  secret values never appear in logs/error strings.
- Prelude mechanism: registering the test mail prelude makes `mail-drafts |> SEND` resolve
  (via t06) and **desugar to** `… |> CALL mail.send` (plan/golden assertion); an alias body
  that is not `-> Plan` yields `ImpureAlias`; a within-prelude name clash yields
  `DuplicateAlias`; the same alias name on two drivers stays scoped (no global clash).
- All behavioral assertions are pure evaluation / resolved-AST / plan assertions — no
  effect execution and no live services.
