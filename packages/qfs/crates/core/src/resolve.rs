//! Name resolution (RFD-0001 §3, ticket t06) — the semantic pass that sits between
//! the parsed AST (`qfs-parser`, t04) and the typed schema model (`qfs-types`, t05),
//! turning raw identifiers into resolved registry references.
//!
//! This is where the reserved **`qfs-core → qfs-parser`** edge is wired (acceptance
//! criterion C5): the resolver consumes `qfs_parser::Statement` and resolves three
//! identifier classes against the open registries, adding **zero** keywords to the
//! frozen core:
//!
//! 1. **`CALL driver.action(args)`** — routes the `driver` namespace through the
//!    [`MountRegistry`] longest-prefix router to a `Driver`, then resolves the action
//!    against that driver's declared [`procedures()`](qfs_driver::Driver::procedures)
//!    (`resolve_proc`). Unknown driver / unknown proc / arity / param-name mismatches
//!    each become a structured [`ResolveError`].
//! 2. **Receiver-typed pure aliases** (`SEND`, `MERGE`) — pure registry functions
//!    (never keywords) shipped in a driver's [`prelude()`](qfs_driver::Driver::prelude).
//!    An alias is in scope only for a pipeline whose **receiver driver** ships it; it
//!    desugars to the underlying qualified `CALL` (`d |> SEND` → `d |> CALL mail.send`),
//!    preserving the source span.
//! 3. **Capability gating** — an effect verb (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`) is
//!    checked against the target node's [`Capabilities`](qfs_driver::Capabilities) via
//!    [`check_capability`](qfs_driver::check_capability) so an unsupported verb fails
//!    *before* a `Plan` exists.
//!
//! ## Purity invariant (RFD §3, the safety property)
//! Every resolved callable is plan-constructing (`… -> Plan`); `CALL`/alias build a
//! `Plan`, they never perform. This module is pure data-in / data-out: it makes **no**
//! driver network calls, reads only the read-only registry, and is unit-testable in
//! isolation. The receiver-typing rule fails **closed** — a multi-/unknown-receiver
//! alias use is rejected ([`ResolveError::AmbiguousAlias`]/[`ResolveError::UnknownReceiver`])
//! rather than guessed.
//!
//! ## Canonical effect-verb mapping (t09 carry-over O2)
//! The canonical `qfs_parser::EffectVerb → qfs_plan::WriteVerb` translation lives here
//! ([`write_verb_for`]) as an **exhaustive match with no `_` arm**, so a future verb
//! cannot be silently dropped. The verb's [`Verb`](qfs_driver::Verb) (for capability
//! gating) is derived from the same total match.

use qfs_driver::{check_capability, resolve_proc, CfsError, Driver, Path, Verb};
use qfs_parser::{
    CallRef, EffectStmt, EffectVerb, Expr, FnRef, NamedArg, PathExpr, PipeOp, Pipeline, PlanWrap,
    Projection, Source, Statement,
};
use qfs_plan::WriteVerb;
use qfs_types::DriverId;

use crate::registry::{MountRegistry, Realm};

/// A resolved, namespaced callable identity — what a `CALL` or a desugared alias binds
/// to (RFD §3). `driver` is the plan [`DriverId`] (e.g. `mail`); `proc` is the
/// unqualified action name (e.g. `send`); `qualified` is the registry key (`mail.send`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
    /// The driver the call routes to (plan identity).
    pub driver: DriverId,
    /// The unqualified procedure name (e.g. `send`).
    pub proc: String,
    /// The qualified registry name (`driver.proc`, e.g. `mail.send`).
    pub qualified: String,
    /// Whether the resolved procedure is irreversible (carried for E2 PREVIEW/POLICY).
    pub irreversible: bool,
}

impl ResolvedCall {
    fn new(driver: DriverId, proc: &str, irreversible: bool) -> Self {
        let qualified = format!("{}.{}", driver.as_str(), proc);
        Self {
            driver,
            proc: proc.to_string(),
            qualified,
            irreversible,
        }
    }
}

/// The structured, machine-readable outcome of name resolution (RFD §5: errors are
/// parseable by an AI, not prose). Every arm carries the actionable context — the
/// available procedures, the candidate drivers — an AI needs to recover without
/// string-parsing. Credentials/secrets never appear here (RFD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveError {
    /// A `CALL`'s `driver` namespace has no registered mount (the path router found no
    /// driver). Carries the namespace the caller used.
    UnknownDriver {
        /// The driver namespace from the `CALL` (e.g. `drive`).
        driver: String,
    },
    /// A `CALL driver.proc` where the driver exists but does not declare `proc`
    /// (capability gate, RFD §3). Carries the driver's available procedures so the AI
    /// can pick a real one.
    UnknownProcedure {
        /// The driver namespace.
        driver: String,
        /// The unknown procedure name.
        name: String,
        /// The procedures the driver *does* declare (for AI recovery).
        available: Vec<String>,
    },
    /// A `CALL` supplied the wrong number of positional arguments for the procedure.
    ArityMismatch {
        /// The qualified procedure (`driver.proc`).
        qualified: String,
        /// The number of parameters the procedure declares.
        expected: usize,
        /// The number of arguments the call supplied.
        found: usize,
    },
    /// A `CALL` named an argument the procedure does not declare. Carries the declared
    /// parameter names so the AI can correct the keyword.
    UnknownArg {
        /// The qualified procedure (`driver.proc`).
        qualified: String,
        /// The unknown argument name the call used.
        arg: String,
        /// The parameter names the procedure declares.
        params: Vec<String>,
    },
    /// An alias (e.g. `SEND`) used in a pipeline whose **receiver driver** does not ship
    /// it in its prelude (receiver-typed resolution, RFD §3).
    AliasNotProvided {
        /// The alias surface name.
        name: String,
        /// The receiver driver that lacked the alias.
        driver: String,
    },
    /// An alias resolvable on more than one in-scope driver — fail closed and direct
    /// the user to the qualified `CALL` (RFD §3 ambiguity policy).
    AmbiguousAlias {
        /// The alias surface name.
        name: String,
        /// The candidate drivers that all ship the alias.
        candidates: Vec<String>,
    },
    /// An alias was used but the pipeline's receiver driver could not be determined
    /// (the source is not a single resolved `/driver/...` path) — fail closed rather
    /// than guess (RFD §3 receiver typing).
    UnknownReceiver {
        /// The alias surface name whose receiver could not be resolved.
        name: String,
    },
    /// A pipeline read `FROM <name>` a bare identifier that is **not** a `LET` binding in
    /// scope (M6, ticket t60). A typo in a bound name is a structured, AI-consumable error
    /// here — never a silent empty relation. Carries the unbound name the pipeline used.
    UnknownBinding {
        /// The bare-identifier source with no matching `LET` binding in scope.
        name: String,
    },
    /// An effect verb was planned against a node whose driver does not declare it — the
    /// resolve-time capability gate (RFD §5). Wraps the structured driver-side error.
    UnsupportedVerb {
        /// The target path.
        path: String,
        /// The rejected verb's stable label.
        verb: &'static str,
        /// The verbs the node *does* support (for AI recovery).
        supported: Vec<&'static str>,
    },
}

impl ResolveError {
    /// A stable, machine-readable code an AI-facing caller branches on (RFD §5),
    /// distinct per arm.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            ResolveError::UnknownDriver { .. } => "unknown_driver",
            ResolveError::UnknownProcedure { .. } => "unknown_procedure",
            ResolveError::ArityMismatch { .. } => "arity_mismatch",
            ResolveError::UnknownArg { .. } => "unknown_arg",
            ResolveError::AliasNotProvided { .. } => "alias_not_provided",
            ResolveError::AmbiguousAlias { .. } => "ambiguous_alias",
            ResolveError::UnknownReceiver { .. } => "unknown_receiver",
            ResolveError::UnknownBinding { .. } => "unknown_binding",
            ResolveError::UnsupportedVerb { .. } => "unsupported_verb",
        }
    }
}

/// Map a parser [`EffectVerb`] to its canonical [`WriteVerb`] (t09 carry-over O2).
///
/// This is the **single source of truth** for the AST-verb translation, an
/// **exhaustive match with no `_` arm** so a future [`EffectVerb`] variant fails to
/// compile until it is mapped here (it cannot silently drift). [`qfs_plan::kind_for_verb`]
/// then maps the [`WriteVerb`] onto the effect-plan [`EffectKind`](qfs_plan::EffectKind).
#[must_use]
pub fn write_verb_for(verb: EffectVerb) -> WriteVerb {
    match verb {
        EffectVerb::Insert => WriteVerb::Insert,
        EffectVerb::Upsert => WriteVerb::Upsert,
        EffectVerb::Update => WriteVerb::Update,
        EffectVerb::Remove => WriteVerb::Remove,
    }
}

/// Map a parser [`EffectVerb`] to the universal [`Verb`] used for capability gating.
/// Also a total match (no `_` arm), keeping the two verb vocabularies bound.
#[must_use]
pub fn capability_verb_for(verb: EffectVerb) -> Verb {
    match verb {
        EffectVerb::Insert => Verb::Insert,
        EffectVerb::Upsert => Verb::Upsert,
        EffectVerb::Update => Verb::Update,
        EffectVerb::Remove => Verb::Remove,
    }
}

/// The name resolver (RFD §3, t06): a pure pass over a parsed [`Statement`] that binds
/// each `CALL` / prelude alias to a [`ResolvedCall`] and gates effect verbs against the
/// target node's capabilities. Reads the [`MountRegistry`] (longest-prefix path router
/// → driver) read-only; performs no I/O and no driver network calls.
pub struct Resolver<'r> {
    mounts: &'r MountRegistry,
}

impl<'r> Resolver<'r> {
    /// Build a resolver over a mount registry.
    #[must_use]
    pub fn new(mounts: &'r MountRegistry) -> Self {
        Self { mounts }
    }

    /// Resolve every name in a statement. Returns the resolved `CALL`/alias bindings in
    /// pipeline order (for golden/plan assertions and downstream evaluation) or the
    /// first structured [`ResolveError`].
    ///
    /// # Errors
    /// Any unknown driver/procedure, arg mismatch, ill-typed alias, or unsupported verb
    /// surfaces as the corresponding [`ResolveError`] arm.
    pub fn resolve_statement(&self, stmt: &Statement) -> Result<Vec<ResolvedCall>, ResolveError> {
        let mut out = Vec::new();
        let scope = Scope::default();
        self.resolve_statement_into(stmt, &scope, &mut out)?;
        Ok(out)
    }

    fn resolve_statement_into(
        &self,
        stmt: &Statement,
        scope: &Scope,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        match stmt {
            Statement::Query(pipeline) => self.resolve_pipeline(pipeline, scope, out),
            Statement::Effect(effect) => self.resolve_effect(effect, scope, out),
            Statement::Plan(PlanWrap { inner, .. }) => {
                self.resolve_statement_into(inner, scope, out)
            }
            // A `LET` binding (M6, t60): the `value` resolves in the *outer* scope (no
            // recursive/forward reference — `name` is not yet bound), then the `body`
            // resolves with `name` added (shadowing the outer scope if it repeats).
            Statement::Let { name, value, body } => {
                self.resolve_statement_into(value, scope, out)?;
                let inner = scope.with(name);
                self.resolve_statement_into(body, &inner, out)
            }
            // A `TRANSACTION { … }` block (M6, t62): each body member is an effect statement —
            // resolve every one (capability/procedure gate) so a denied verb inside the block
            // fails before a plan exists, exactly as it would outside the block.
            Statement::Transaction { body, .. } => {
                for member in body {
                    self.resolve_statement_into(member, scope, out)?;
                }
                Ok(())
            }
            // Server DDL desugars to `/server/...` effects downstream (later epic). Its
            // optional `DO`/`AS` clauses are themselves statements — resolve those.
            Statement::Ddl(ddl) => {
                if let Some(do_plan) = &ddl.do_plan {
                    self.resolve_statement_into(do_plan, scope, out)?;
                }
                if let Some(as_query) = &ddl.as_query {
                    self.resolve_statement_into(as_query, scope, out)?;
                }
                Ok(())
            }
        }
    }

    /// Resolve a read pipeline. The **receiver driver** (RFD §3 receiver typing) is the
    /// driver of the `FROM` source; alias ops resolve against *its* prelude, threaded
    /// down the `|>` walk.
    fn resolve_pipeline(
        &self,
        pipeline: &Pipeline,
        scope: &Scope,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        // A bare-identifier source must name a `LET` binding in scope (t60) — a typo is a
        // structured error, not a silent empty relation. A **reserved realm** name
        // (decision P / §1.3) is the exception: it resolves to its fixed realm and is
        // always a legal source, never unbound — and never shadowed by a `LET` binding of
        // the same spelling (the realm ranks above the lexical realm).
        if let Source::Name(name) = &pipeline.source {
            if Realm::from_segment(name).is_none() && !scope.contains(name) {
                return Err(ResolveError::UnknownBinding { name: name.clone() });
            }
        }
        let receiver = self.source_receiver(&pipeline.source);
        // A subquery source carries its own nested calls — resolve them first.
        if let Source::Subquery(inner) = &pipeline.source {
            self.resolve_pipeline(inner, scope, out)?;
        }
        for op in &pipeline.ops {
            self.resolve_pipe_op(op, scope, receiver.as_ref(), out)?;
        }
        Ok(())
    }

    /// Resolve one pipe op against the current receiver driver.
    fn resolve_pipe_op(
        &self,
        op: &PipeOp,
        scope: &Scope,
        receiver: Option<&ReceiverDriver>,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        match op {
            PipeOp::Call(call) => {
                out.push(self.resolve_call(call)?);
                Ok(())
            }
            // A nested pipeline (UNION/EXCEPT/INTERSECT/JOIN sub-source) is a fresh
            // receiver context — resolve it with its own source (and the same binding scope).
            PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => {
                self.resolve_pipeline(p, scope, out)
            }
            PipeOp::Join(join) => {
                if let Source::Name(name) = &join.source {
                    if Realm::from_segment(name).is_none() && !scope.contains(name) {
                        return Err(ResolveError::UnknownBinding { name: name.clone() });
                    }
                }
                if let Source::Subquery(inner) = &join.source {
                    self.resolve_pipeline(inner, scope, out)?;
                }
                self.resolve_expr_fns(&join.on, receiver, out)
            }
            PipeOp::Where(expr) => self.resolve_expr_fns(expr, receiver, out),
            PipeOp::GroupBy(exprs) => {
                for e in exprs {
                    self.resolve_expr_fns(e, receiver, out)?;
                }
                Ok(())
            }
            PipeOp::OrderBy(keys) => {
                for k in keys {
                    self.resolve_expr_fns(&k.expr, receiver, out)?;
                }
                Ok(())
            }
            PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
                for p in projs {
                    if let Projection::Expr { expr, .. } = p {
                        self.resolve_alias_in_expr(expr, receiver, out)?;
                    }
                }
                Ok(())
            }
            PipeOp::Extend(asgns) | PipeOp::Set(asgns) => {
                for a in asgns {
                    self.resolve_alias_in_expr(&a.value, receiver, out)?;
                }
                Ok(())
            }
            // Pure structural / codec ops carry no callables to resolve here.
            PipeOp::Limit(_)
            | PipeOp::Distinct
            | PipeOp::As(_)
            | PipeOp::Expand(_)
            | PipeOp::Decode(_)
            | PipeOp::Encode(_) => Ok(()),
        }
    }

    /// Resolve the receiver-typed alias use that appears as a bare `FnRef` in expression
    /// position (an alias call `SEND(receiver)` desugars to a `CALL`). Core/registry
    /// `fn(...)` calls that are not preludes are left for the function-registry ticket;
    /// here we resolve only names a receiver driver ships as a prelude alias.
    fn resolve_alias_in_expr(
        &self,
        expr: &Expr,
        receiver: Option<&ReceiverDriver>,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        self.resolve_expr_fns(expr, receiver, out)
    }

    /// Walk an expression for prelude-alias `FnRef`s and resolve each against the
    /// receiver. Non-alias function names are ignored here (function-registry ticket).
    fn resolve_expr_fns(
        &self,
        expr: &Expr,
        receiver: Option<&ReceiverDriver>,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        match expr {
            Expr::Fn(fnref) => {
                if let Some(resolved) = self.resolve_alias(fnref, receiver)? {
                    out.push(resolved);
                }
                for a in &fnref.args {
                    self.resolve_expr_fns(a, receiver, out)?;
                }
                Ok(())
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr_fns(lhs, receiver, out)?;
                self.resolve_expr_fns(rhs, receiver, out)
            }
            Expr::Unary { expr, .. } => self.resolve_expr_fns(expr, receiver, out),
            Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
                self.resolve_expr_fns(expr, receiver, out)?;
                for e in set {
                    self.resolve_expr_fns(e, receiver, out)?;
                }
                Ok(())
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.resolve_expr_fns(expr, receiver, out)?;
                self.resolve_expr_fns(low, receiver, out)?;
                self.resolve_expr_fns(high, receiver, out)
            }
            Expr::Like { expr, pattern } => {
                self.resolve_expr_fns(expr, receiver, out)?;
                self.resolve_expr_fns(pattern, receiver, out)
            }
            // A lambda body (M6, t61) is walked for nested prelude-alias `fn`s like any
            // sub-expression; its parameters introduce no callable to resolve here (a lambda
            // is a pure value, never an effect — RFD §3 purity).
            Expr::Lambda { body, .. } => self.resolve_expr_fns(body, receiver, out),
            // t92 composite constructors: walk each element/field sub-expression for nested
            // prelude-alias `fn`s (a struct field may be `{ x: driver.alias(col) }`).
            Expr::Array(elems) => {
                for e in elems {
                    self.resolve_expr_fns(e, receiver, out)?;
                }
                Ok(())
            }
            Expr::Struct(fields) => {
                for (_, e) in fields {
                    self.resolve_expr_fns(e, receiver, out)?;
                }
                Ok(())
            }
            Expr::Lit(_) | Expr::Col(_) | Expr::Path(_) => Ok(()),
        }
    }

    /// Resolve a qualified `CALL driver.action(args)` against the driver's declared
    /// procedures. Routes `driver` through the mount registry; checks arity + named-arg
    /// param names. Returns the structured error for each miss class.
    fn resolve_call(&self, call: &CallRef) -> Result<ResolvedCall, ResolveError> {
        let driver = self.resolve_driver_namespace(&call.driver).ok_or_else(|| {
            ResolveError::UnknownDriver {
                driver: call.driver.clone(),
            }
        })?;

        let sig = resolve_proc(driver.as_ref(), &call.action).map_err(|err| match err {
            CfsError::UnknownProcedure(_) => ResolveError::UnknownProcedure {
                driver: call.driver.clone(),
                name: call.action.clone(),
                available: driver.procedures().iter().map(|p| p.name.clone()).collect(),
            },
            // `resolve_proc` only ever returns `UnknownProcedure`; any other arm would
            // be a contract change, so surface it as an unknown-procedure miss rather
            // than panic (lib code stays panic-free).
            _ => ResolveError::UnknownProcedure {
                driver: call.driver.clone(),
                name: call.action.clone(),
                available: Vec::new(),
            },
        })?;

        let qualified = format!("{}.{}", driver.id().as_str(), call.action);
        self.check_args(&qualified, &call.args, sig)?;

        Ok(ResolvedCall::new(
            driver.id(),
            &call.action,
            sig.irreversible,
        ))
    }

    /// Arity + named-arg param-name check for a `CALL`. Positional args must not exceed
    /// the declared param count; every `name => value` arg must name a declared param.
    fn check_args(
        &self,
        qualified: &str,
        args: &[NamedArg],
        sig: &qfs_driver::ProcSig,
    ) -> Result<(), ResolveError> {
        let positional = args.iter().filter(|a| a.name.is_none()).count();
        let named = args.len() - positional;
        // Total supplied args must not exceed the declared params (a named arg fills a
        // declared slot; a positional fills a declared slot positionally).
        if positional + named > sig.params.len() {
            return Err(ResolveError::ArityMismatch {
                qualified: qualified.to_string(),
                expected: sig.params.len(),
                found: args.len(),
            });
        }
        for arg in args {
            if let Some(name) = &arg.name {
                if !sig.params.iter().any(|p| &p.name == name) {
                    return Err(ResolveError::UnknownArg {
                        qualified: qualified.to_string(),
                        arg: name.clone(),
                        params: sig.params.iter().map(|p| p.name.clone()).collect(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Resolve a receiver-typed prelude alias `FnRef`. Returns:
    /// - `Ok(None)` if `fnref.name` is not an alias on *any* registered driver (it is a
    ///   core/registry function for a later ticket, not our concern).
    /// - `Ok(Some(resolved))` if exactly the receiver ships it (desugared `CALL`).
    /// - `Err(...)` for ambiguous / not-provided / unknown-receiver alias use.
    fn resolve_alias(
        &self,
        fnref: &FnRef,
        receiver: Option<&ReceiverDriver>,
    ) -> Result<Option<ResolvedCall>, ResolveError> {
        // Which registered drivers ship an alias with this surface name?
        let providers = self.alias_providers(&fnref.name);
        if providers.is_empty() {
            // Not a prelude alias at all — leave it for the function-registry ticket.
            return Ok(None);
        }
        // It IS an alias somewhere. Receiver typing now decides whether *this* use is
        // legal. Fail closed if the receiver is unknown.
        let Some(receiver) = receiver else {
            return Err(ResolveError::UnknownReceiver {
                name: fnref.name.clone(),
            });
        };

        // Does the receiver itself ship it? If yes, that is the binding (even if other
        // drivers also ship it — receiver typing disambiguates).
        let on_receiver = receiver
            .driver
            .prelude()
            .iter()
            .find(|a| a.name == fnref.name);
        if let Some(alias) = on_receiver {
            let (_, proc) = split_qualified(&alias.desugars_to);
            let irreversible = resolve_proc(receiver.driver.as_ref(), proc)
                .map(|s| s.irreversible)
                .unwrap_or(false);
            return Ok(Some(ResolvedCall::new(
                receiver.driver.id(),
                proc,
                irreversible,
            )));
        }

        // The receiver does NOT ship it. If multiple other drivers do, that is the
        // classic ambiguity the qualified `CALL` resolves; otherwise it is simply not
        // provided by the receiver.
        if providers.len() > 1 {
            return Err(ResolveError::AmbiguousAlias {
                name: fnref.name.clone(),
                candidates: providers,
            });
        }
        Err(ResolveError::AliasNotProvided {
            name: fnref.name.clone(),
            driver: receiver.driver.id().as_str().to_string(),
        })
    }

    /// All registered drivers that ship a prelude alias with `name`, by plan id
    /// (deterministic order — the registry iterates a `BTreeMap`).
    fn alias_providers(&self, name: &str) -> Vec<String> {
        self.mounts
            .drivers()
            .filter(|d| d.prelude().iter().any(|a| a.name == name))
            .map(|d| d.id().as_str().to_string())
            .collect()
    }

    /// Determine the receiver driver of a pipeline source (RFD §3 receiver typing). Only
    /// a single resolved `/driver/...` path yields a receiver; `VALUES` and multi-driver
    /// subqueries yield `None` (alias use then fails closed).
    fn source_receiver(&self, source: &Source) -> Option<ReceiverDriver> {
        match source {
            Source::Path(path) => self.path_receiver(path),
            // VALUES has no driver; a subquery's receiver is itself ambiguous in the
            // general (multi-driver) case; a `LET`-bound name's receiver is the bound
            // relation's (not tracked here) — all fail closed for alias purposes (t60).
            Source::Values(_) | Source::Subquery(_) | Source::Name(_) => None,
        }
    }

    /// Resolve a `/driver/...` path's driver through the longest-prefix mount router.
    fn path_receiver(&self, path: &PathExpr) -> Option<ReceiverDriver> {
        let full = render_mount_path(path);
        self.mounts
            .resolve_path(&full)
            .map(|(driver, _sub)| ReceiverDriver { driver })
    }

    /// Resolve a `CALL`'s `driver` namespace token (e.g. `mail`) to a registered driver.
    /// Tries the mount router on `/<namespace>` first (the conventional mount), so the
    /// namespace need not repeat the leading slash at the call site.
    fn resolve_driver_namespace(&self, namespace: &str) -> Option<std::sync::Arc<dyn Driver>> {
        let mount = format!("/{namespace}");
        self.mounts
            .resolve_path(&mount)
            .map(|(driver, _)| driver)
            // Fall back to an exact mount match for drivers mounted under a literal name.
            .or_else(|| self.mounts.resolve(&mount).ok())
            // Fall back to a driver whose CANONICAL id() matches the namespace (t100030). A `CALL`
            // routes by driver IDENTITY, and id() stays canonical (the keystone decision) even when
            // `CONNECT` mounts the driver under a MULTI-SEGMENT defined path (`/work/orders`) rather
            // than its canonical single-segment name — so `CALL postgres.foo()` finds the postgres
            // driver wherever it was mounted, not only at `/postgres`. Deterministic (mount order);
            // when two connections share a driver id the first-mounted answers the bare-id CALL.
            .or_else(|| {
                self.mounts
                    .drivers()
                    .find(|d| d.id().as_str() == namespace)
                    .map(std::sync::Arc::clone)
            })
    }

    /// Gate an effect statement's verb against the target node's capabilities (RFD §5).
    fn resolve_effect(
        &self,
        effect: &EffectStmt,
        scope: &Scope,
        out: &mut Vec<ResolvedCall>,
    ) -> Result<(), ResolveError> {
        // The canonical, exhaustive verb mappings (t09 O2) — referenced so a new verb
        // forces both maps to be updated.
        let _write = write_verb_for(effect.verb);
        let verb = capability_verb_for(effect.verb);

        let full = render_mount_path(&effect.target);
        if let Some((driver, sub)) = self.mounts.resolve_path(&full) {
            let path = Path::new(format!("/{}/{}", driver.id().as_str(), sub));
            check_capability(driver.as_ref(), &path, verb).map_err(|err| match err {
                CfsError::UnsupportedVerb {
                    path,
                    verb,
                    supported,
                } => ResolveError::UnsupportedVerb {
                    path,
                    verb,
                    supported,
                },
                // `check_capability` only ever yields `UnsupportedVerb`; treat any other
                // arm as an empty-supported unsupported verb rather than panic.
                _ => ResolveError::UnsupportedVerb {
                    path: full.clone(),
                    verb: verb.label(),
                    supported: Vec::new(),
                },
            })?;
        }
        // An unrouted target path is a path/mount-resolution concern (deferred, ticket
        // scope); no callable to resolve. Effect bodies may contain a sub-pipeline.
        if let qfs_parser::EffectBody::Pipeline(p) = &effect.body {
            self.resolve_pipeline(p, scope, out)?;
        }
        Ok(())
    }
}

/// A lexical binding scope for `LET` (M6, ticket t60): the set of names bound and in scope
/// at a point in the program. Resolution consults it **before** the mount registry so a
/// bound name resolves to its relation rather than being mistaken for a mount path. Cheap to
/// extend by value ([`Scope::with`]) so each `LET` body gets its own immutable scope —
/// shadowing is a plain re-insert, and the parent scope is never mutated.
#[derive(Debug, Clone, Default)]
struct Scope {
    names: std::collections::BTreeSet<String>,
}

impl Scope {
    /// A new scope with `name` added (shadowing any same-named outer binding). Returns an
    /// owned child so the parent is untouched (lexical, non-recursive scoping).
    fn with(&self, name: &str) -> Self {
        let mut names = self.names.clone();
        names.insert(name.to_string());
        Self { names }
    }

    /// Whether `name` is a binding in scope.
    fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

/// The receiver driver context threaded down a pipeline's `|>` walk (RFD §3 receiver
/// typing). Holds the resolved driver of the upstream `FROM` source.
struct ReceiverDriver {
    driver: std::sync::Arc<dyn Driver>,
}

/// Render a parser [`PathExpr`] back into a `/seg/seg` mount path string for the router
/// (`@version` / globs are addressing concerns dropped here — t06 resolves *names*).
fn render_mount_path(path: &PathExpr) -> String {
    let mut s = String::new();
    for seg in &path.segments {
        s.push('/');
        s.push_str(&seg.name);
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}

/// Split a qualified `driver.proc` into its `(driver, proc)` halves. A name with no dot
/// is treated as an unqualified proc with an empty driver half.
fn split_qualified(qualified: &str) -> (&str, &str) {
    match qualified.split_once('.') {
        Some((driver, proc)) => (driver, proc),
        None => ("", qualified),
    }
}

#[cfg(test)]
mod tests;
