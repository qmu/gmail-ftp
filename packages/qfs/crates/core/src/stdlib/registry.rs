//! The [`StdlibRegistry`] — populates the function registry with the core stdlib and
//! merges driver **preludes** (RFD-0001 §3/§5, ticket t08).
//!
//! This is the registry t06's name resolution resolves `fn(...)` against (the second open
//! registry). It holds the core built-ins (scalar/aggregate/table-valued) and, for each
//! driver, the receiver-typed prelude aliases the driver ships — **namespaced by
//! [`DriverId`]**, never flattened into the core namespace, so receiver-typed resolution
//! stays collision-proof (the same alias on two drivers is fine; a duplicate *within* one
//! prelude is a [`PreludeError::Duplicate`]).
//!
//! ## Purity invariant at registration (RFD §3)
//! A prelude's alias body is qfs source (`d |> CALL mail.send`). [`StdlibRegistry::
//! register_prelude`] parses each body and asserts it is **plan-constructing** (a single
//! `CALL` desugaring) — a body that is not yields [`PreludeError::Impure`], so a prelude
//! can never smuggle an impure or non-`-> Plan` alias into scope. The resulting
//! [`ResolvedAlias`] is exactly the `AliasFn` shape t06 already resolves against.

use std::collections::BTreeMap;

use qfs_driver::AliasFn;
use qfs_parser::{parse_statement, PipeOp, Source, Statement};
use qfs_types::DriverId;

use super::aggregate::aggregate_builtins;
use super::context::context_builtins;
use super::higher_order::higher_order_builtins;
use super::scalar::scalar_builtins;
use super::tablevalued::table_valued_builtins;
use super::BuiltinFn;

/// One alias a driver declares in its prelude (RFD §5). The `body` is **qfs source** (a
/// pipeline like `d |> CALL mail.send`); registration parses it and verifies it desugars
/// to a single `CALL` (the purity invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasDecl {
    /// The alias surface name (e.g. `SEND`).
    pub name: String,
    /// The qfs-source body the alias desugars to (e.g. `d |> CALL mail.send`).
    pub body: String,
}

impl AliasDecl {
    /// Construct an alias declaration from a name and a qfs-source body.
    #[must_use]
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            body: body.into(),
        }
    }
}

/// What a driver ships alongside its procedures (RFD §5 Prelude). A set of receiver-typed
/// pure aliases, owned by the driver's [`DriverId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prelude {
    /// The driver these aliases belong to (their receiver scope).
    pub driver: DriverId,
    /// The alias declarations (qfs-source bodies).
    pub aliases: Vec<AliasDecl>,
}

impl Prelude {
    /// Construct a prelude for a driver.
    #[must_use]
    pub fn new(driver: DriverId, aliases: Vec<AliasDecl>) -> Self {
        Self { driver, aliases }
    }
}

/// A parsed, purity-checked prelude alias (RFD §3) — the `(name, desugars_to)` binding
/// t06 resolves a receiver-typed alias use against. Convertible to the driver crate's
/// [`AliasFn`] so the resolver surface is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAlias {
    /// The driver whose prelude shipped the alias (receiver scope).
    pub driver: DriverId,
    /// The alias surface name.
    pub name: String,
    /// The qualified procedure it desugars to (e.g. `mail.send`).
    pub desugars_to: String,
}

impl ResolvedAlias {
    /// The driver-crate [`AliasFn`] view of this alias (t06's resolution surface).
    #[must_use]
    pub fn as_alias_fn(&self) -> AliasFn {
        AliasFn::new(self.name.clone(), self.desugars_to.clone())
    }
}

/// The structured error a prelude registration can raise (RFD §5). Carries the driver +
/// alias so an AI can localise the fault; never a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreludeError {
    /// An alias body failed to parse as qfs source. Carries the parser error code (the
    /// full [`qfs_parser::ParseError`] is not `Eq`, so we keep the stable code).
    Parse {
        /// The owning driver.
        driver: String,
        /// The alias name.
        name: String,
        /// The parser's stable error code.
        code: &'static str,
    },
    /// An alias body parsed but is **not** a single plan-constructing `CALL` (purity
    /// invariant). It does not desugar to `-> Plan`, so it is rejected.
    Impure {
        /// The owning driver.
        driver: String,
        /// The alias name.
        name: String,
    },
    /// Two aliases **within one prelude** share a name. (The same name across *different*
    /// drivers is fine — aliases are receiver-scoped.)
    Duplicate {
        /// The owning driver.
        driver: String,
        /// The duplicated alias name.
        name: String,
    },
}

impl PreludeError {
    /// A stable, machine-readable code (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            PreludeError::Parse { .. } => "prelude_alias_parse",
            PreludeError::Impure { .. } => "prelude_impure_alias",
            PreludeError::Duplicate { .. } => "prelude_duplicate_alias",
        }
    }
}

/// The function registry: the core stdlib built-ins + the merged, namespaced driver
/// preludes (RFD §3, ticket t08). The surface t06 resolves `fn(...)` against.
#[derive(Default)]
pub struct StdlibRegistry {
    /// Core built-ins keyed by surface name (`BTreeMap` for deterministic iteration —
    /// test stability, mirroring the other registries).
    core: BTreeMap<String, BuiltinFn>,
    /// Driver preludes keyed by `DriverId`; each value is the driver's parsed, namespaced
    /// aliases.
    preludes: BTreeMap<String, Vec<ResolvedAlias>>,
}

impl StdlibRegistry {
    /// An empty registry (no core, no preludes).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with every core built-in (scalar/path/date/number/context/
    /// aggregate/table-valued). This is the default the engine resolves `fn(...)` through;
    /// a driver extends it only with receiver-scoped preludes (a new fn family is a
    /// stdlib edit, by design — the core vocabulary is small and stable, RFD §3).
    #[must_use]
    pub fn with_core() -> Self {
        let mut reg = Self::new();
        for f in core_builtins() {
            reg.core.insert(f.name.clone(), f);
        }
        reg
    }

    /// Look up a core built-in by surface name.
    #[must_use]
    pub fn builtin(&self, name: &str) -> Option<&BuiltinFn> {
        self.core.get(name)
    }

    /// Whether `name` is a registered core built-in.
    #[must_use]
    pub fn is_builtin(&self, name: &str) -> bool {
        self.core.contains_key(name)
    }

    /// Whether `name` is a registered **aggregate** built-in (for aggregate-vs-scalar
    /// dispatch — using an aggregate outside `AGGREGATE` is a typed error, not a panic).
    #[must_use]
    pub fn is_aggregate(&self, name: &str) -> bool {
        self.core.get(name).is_some_and(BuiltinFn::is_aggregate)
    }

    /// The number of registered core built-ins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.core.len()
    }

    /// Whether the core is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }

    /// Register a driver's prelude: parse + purity-check each alias body, detect duplicates
    /// **within the prelude**, and merge the resulting aliases namespaced by the driver's
    /// [`DriverId`] (RFD §3 receiver scoping).
    ///
    /// # Errors
    /// [`PreludeError::Parse`] if a body is not valid qfs; [`PreludeError::Impure`] if a
    /// body does not desugar to a single plan-constructing `CALL`;
    /// [`PreludeError::Duplicate`] if two aliases in this prelude share a name.
    pub fn register_prelude(&mut self, prelude: &Prelude) -> Result<(), PreludeError> {
        let driver = prelude.driver.as_str().to_string();
        let mut resolved: Vec<ResolvedAlias> = Vec::with_capacity(prelude.aliases.len());
        for decl in &prelude.aliases {
            // Duplicate WITHIN this prelude → error (across drivers is fine; scoped).
            if resolved.iter().any(|a| a.name == decl.name) {
                return Err(PreludeError::Duplicate {
                    driver: driver.clone(),
                    name: decl.name.clone(),
                });
            }
            let desugars_to = parse_alias_body(&driver, &decl.name, &decl.body)?;
            resolved.push(ResolvedAlias {
                driver: prelude.driver.clone(),
                name: decl.name.clone(),
                desugars_to,
            });
        }
        self.preludes.insert(driver, resolved);
        Ok(())
    }

    /// The parsed, purity-checked aliases a driver's prelude contributed (empty if the
    /// driver registered none). Receiver-scoped: an alias is in scope only for plans whose
    /// receiver is this driver.
    #[must_use]
    pub fn prelude_aliases(&self, driver: &DriverId) -> &[ResolvedAlias] {
        self.preludes
            .get(driver.as_str())
            .map_or(&[], Vec::as_slice)
    }

    /// Every driver that ships a prelude alias with surface `name`, by [`DriverId`] string
    /// (deterministic order). Used by t06's ambiguity/receiver-typing decision — the same
    /// alias on two drivers stays scoped (no global clash).
    #[must_use]
    pub fn alias_providers(&self, name: &str) -> Vec<String> {
        self.preludes
            .iter()
            .filter(|(_, aliases)| aliases.iter().any(|a| a.name == name))
            .map(|(driver, _)| driver.clone())
            .collect()
    }

    /// The driver-crate [`AliasFn`] view of a driver's prelude (the slice t06's
    /// receiver-typed resolution consumes via `Driver::prelude()`).
    #[must_use]
    pub fn prelude_alias_fns(&self, driver: &DriverId) -> Vec<AliasFn> {
        self.prelude_aliases(driver)
            .iter()
            .map(ResolvedAlias::as_alias_fn)
            .collect()
    }
}

/// Parse one prelude alias body as qfs source and verify it is a single plan-constructing
/// `CALL` pipeline, returning the qualified `driver.proc` it desugars to. This is the
/// purity gate: anything that is not a lone `… |> CALL d.p` body is rejected.
fn parse_alias_body(driver: &str, name: &str, body: &str) -> Result<String, PreludeError> {
    let stmt = parse_statement(body).map_err(|e| PreludeError::Parse {
        driver: driver.to_string(),
        name: name.to_string(),
        code: e.code.as_str(),
    })?;
    // The body must be a query pipeline whose ops contain exactly one `CALL` (the
    // desugaring target) and no effectful / non-plan op. A receiver source is allowed
    // (`d |> CALL …` parses `d` as the source).
    let Statement::Query(pipeline) = &stmt else {
        return Err(PreludeError::Impure {
            driver: driver.to_string(),
            name: name.to_string(),
        });
    };
    // A bare source with no CALL, or a multi-CALL / non-CALL-op body, is not a pure
    // single-CALL alias.
    let calls: Vec<&qfs_parser::CallRef> = pipeline
        .ops
        .iter()
        .filter_map(|op| match op {
            PipeOp::Call(c) => Some(c),
            _ => None,
        })
        .collect();
    let only_calls = pipeline.ops.iter().all(|op| matches!(op, PipeOp::Call(_)));
    // The source must be a plain path/identifier receiver (`d`), not VALUES/subquery.
    let plain_source = matches!(pipeline.source, Source::Path(_));
    if calls.len() != 1 || !only_calls || !plain_source {
        return Err(PreludeError::Impure {
            driver: driver.to_string(),
            name: name.to_string(),
        });
    }
    let call = calls[0];
    Ok(format!("{}.{}", call.driver, call.action))
}

/// The full list of core built-ins, assembled from each family. Stable order (each family
/// is name-ordered; the registry re-keys by a `BTreeMap`).
fn core_builtins() -> Vec<BuiltinFn> {
    let mut all = scalar_builtins();
    all.extend(context_builtins());
    all.extend(aggregate_builtins());
    all.extend(table_valued_builtins());
    all.extend(higher_order_builtins());
    all
}

/// Classify a `fn(name, ...)` use against the registry: is it a known function (core
/// built-in or a receiver-scoped prelude alias) or unknown? Pure helper — no panic on any
/// input. (The evaluator's [`Evaluator::type_of_fn`](crate::Evaluator::type_of_fn) is the
/// type-bearing form; this is the membership-only check used by the registry's own tests.)
///
/// # Errors
/// [`FnError::UnknownFunction`] if `name` is neither a core built-in nor a prelude alias on
/// any driver.
#[cfg(test)]
pub(crate) fn classify_fn(reg: &StdlibRegistry, name: &str) -> Result<(), super::FnError> {
    if reg.is_builtin(name) || !reg.alias_providers(name).is_empty() {
        Ok(())
    } else {
        Err(super::FnError::UnknownFunction {
            name: name.to_string(),
        })
    }
}
