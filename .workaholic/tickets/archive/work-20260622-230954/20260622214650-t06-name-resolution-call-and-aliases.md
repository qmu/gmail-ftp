---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: 8dcc89c
category: Added
depends_on: [20260622214650-t04-grammar-ast-and-governance.md, 20260622214650-t05-type-schema-model.md]
---

# Name resolution: CALL procedures + receiver-typed pure aliases

## Overview
This ticket implements the **name-resolution** layer that sits between the parsed AST
(ticket t04) and the typed schema model (ticket t05), turning raw identifiers into
resolved registry references. It realizes RFD §3 ("closed core + three open registries")
and §3 "Universal verbs vs domain actions" / "Purity invariant", plus the driver
contract's **Procedures** and **Prelude** declarations (RFD §5).

Three identifier classes must resolve against the open registries without adding any
keywords to the frozen core:
1. `CALL driver.proc(...)` — resolves **only** procedures a driver *declares* as a
   capability. Namespacing keeps `git.merge` ≠ `github.merge` collision-proof.
2. Ergonomic aliases (`SEND`, `MERGE`) — **pure registry functions** (not keywords),
   in scope only for plans whose receiver driver ships them in its prelude
   (receiver-typed resolution). Ambiguity falls back to the qualified `CALL`.
3. Core/registry `fn(...)` calls — resolved by signature.

The invariant we enforce here: **every function (core or alias) has type `… -> Plan`**.
Resolution rejects any callable that is not pure-plan-constructing.

## Scope
In scope:
- A `Resolver` that walks the AST and binds each `CallExpr` / `ProcCall` / alias to a
  `ResolvedRef` against the **procedure/function registry** keyed by driver namespace.
- Receiver-typed alias resolution: determine the receiving plan's driver, look up its
  prelude aliases, desugar `d |> SEND` → `d |> CALL mail.send`.
- Capability gating for `CALL`: reject `CALL driver.proc` when the driver does not
  declare `proc` (structured, AI-readable error).
- Purity-invariant check: every resolved callable must have a `-> Plan` return type.
- Ambiguity policy: alias resolvable on >1 in-scope driver → error directing to qualified `CALL`.

Out of scope (deferred):
- Path/mount resolution for `/driver/...` (ticket t04 grammar / driver registry wiring).
- Actual `Plan` DAG construction & execution semantics → **effect-plan/runtime epic E2**.
- Type-checking column/argument schemas beyond the `-> Plan` return shape → **t05** does
  the schema model; this ticket only consumes its `Type`/`Plan` markers.
- Codec (`DECODE`/`ENCODE`) registry resolution → separate codec-registry ticket.
- Pushdown planning → E3 federation.

## Key components
New crate/module `cfs-resolve` (or `crates/lang/src/resolve.rs`), depending on
`cfs-ast` (t04) and `cfs-types` (t05). No vendor types leak in — drivers register via
owned DTOs only.

```rust
/// Identity of a registered callable, namespaced by driver.
pub struct Qualified { pub driver: DriverId, pub name: Ident }

pub enum CallableKind { Procedure, AliasFn, CoreFn }

pub struct ResolvedRef {
    pub qualified: Qualified,
    pub kind: CallableKind,
    pub sig: FnSig,            // from t05; return type MUST be Type::Plan
}

/// Registry surface the resolver reads (populated by Driver declarations, RFD §5).
pub trait CallRegistry {
    fn procedures(&self, driver: &DriverId) -> &[ProcDecl];      // capability list
    fn prelude_aliases(&self, driver: &DriverId) -> &[AliasFn];  // pure SEND/MERGE
    fn core_fns(&self) -> &[FnDecl];
}

pub struct Resolver<'r, R: CallRegistry> { reg: &'r R }

pub enum ResolveError {
    UnknownProcedure { driver: DriverId, name: Ident, available: Vec<Ident> },
    UncapableDriver  { driver: DriverId, name: Ident },
    AmbiguousAlias   { name: Ident, candidates: Vec<DriverId> }, // → use CALL
    AliasNotProvided { name: Ident, driver: DriverId },
    ImpureCallable   { name: Ident, found: Type },               // not `-> Plan`
    UnknownReceiver  { name: Ident },
}

impl<'r, R: CallRegistry> Resolver<'r, R> {
    pub fn resolve_stmt(&self, ast: &Stmt) -> Result<ResolvedStmt, ResolveError>;
}
```

Touches: `cfs-ast` (adds `ResolvedRef` slot to call nodes), `cfs-types` (`Type::Plan`,
`FnSig`), driver-contract trait (read-only here; full driver impls land in E4).

## Implementation steps
1. Define `DriverId`, `Qualified`, `ResolvedRef`, `CallableKind`, `ResolveError`.
2. Define the `CallRegistry` trait + an in-memory `StaticRegistry` test impl seeded with
   `mail.send`, `git.merge`, `github.merge`, and prelude aliases `SEND`(mail), `MERGE`(git).
3. Resolve qualified `CALL driver.proc`: look up `procedures(driver)`; emit
   `UnknownProcedure`/`UncapableDriver` with the available-procs list on miss.
4. Compute the **receiver driver** of an alias call by inspecting the upstream pipe's
   resolved path/driver; thread receiver context through the `|>` walk.
5. Resolve aliases against `prelude_aliases(receiver)`; desugar to the underlying `CALL`
   node, preserving source span for diagnostics.
6. Implement ambiguity detection: if an alias is in scope for multiple candidate drivers,
   return `AmbiguousAlias` pointing the user to qualified `CALL`.
7. Enforce purity: assert `sig.ret == Type::Plan` for every resolved callable; else `ImpureCallable`.
8. Produce a `ResolvedStmt` mirroring the AST with every call node carrying a `ResolvedRef`.
9. Structured-error mapping to the AI-facing error envelope (parse-time rejection, RFD §5).

## Considerations
- **Capability gating = least privilege (RFD §10).** A driver only exposes the irreducible
  transitions it declares; resolution is the choke point that prevents fabricating a `CALL`.
  Keep the available-procs list in errors but never leak credential/secret material.
- **Purity invariant is the safety property (RFD §3).** `SEND`-as-a-function is only safe
  because it constructs, never performs. The `-> Plan` check here is load-bearing for the
  whole "dry-runnable / testable / composable" guarantee; do not weaken it.
- **Idempotency/recovery** is not implemented here, but resolution must preserve the
  proc identity (e.g. `mail.send` being irreversible) so downstream E2 can flag
  `irreversible` and require PREVIEW.
- **Hard part: receiver typing.** Alias scope depends on the *upstream* plan's driver,
  which itself may be a union/cross-source pipe. Resolve aliases bottom-up after the path
  side is bound; if the receiver is multi-driver or unknown, fail closed
  (`UnknownReceiver`/`AmbiguousAlias`) rather than guess.
- **Observability:** every `ResolveError` is structured (machine-parseable) so the AI
  operating procedure gets actionable feedback, not a string.
- **Directory/standards:** keep this a pure, side-effect-free module; no I/O, no driver
  network calls; registry is read-only data. Unit-testable in isolation.

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; no live creds.
- `CALL git.merge(...)` and `CALL github.merge(method=>'squash')` resolve to **distinct**
  `ResolvedRef`s (namespace isolation asserted).
- `CALL drive.merge(...)` (undeclared) returns `UnknownProcedure`/`UncapableDriver` with the
  driver's available-proc list populated.
- `mail-drafts |> SEND` resolves and **desugars to** `… |> CALL mail.send` (golden/plan
  assertion on the desugared AST).
- An alias provided by two in-scope drivers returns `AmbiguousAlias` naming both candidates.
- A callable whose signature is not `-> Plan` is rejected with `ImpureCallable` (purity
  invariant test).
- Golden tests cover: qualified CALL, alias desugaring, ambiguity fallback, capability
  rejection, and purity rejection — all as resolved-AST/plan assertions (no execution).
