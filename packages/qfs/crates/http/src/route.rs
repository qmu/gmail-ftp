//! Route compilation + the route table (t32).
//!
//! [`RoutePattern`] parses a route string (`/items/:id`, `/x/{p}`) into literal + param
//! segments and exposes the declared param **names**. [`compile_endpoint`] turns one
//! [`qfs_server::EndpointDef`] into a compiled [`CompiledRoute`] — rehydrating its pre-parsed
//! query (t31 [`qfs_core::StatementSpec`], NO re-parse), running the **registration-time
//! read-only policy gate** (a write-lowering endpoint is REFUSED here), and recording the
//! method/pattern/query/param-names. [`Router`] is the table the [`crate::HttpBinding`]
//! hot-swaps: matching a request to a compiled route + extracting its path params.

use std::collections::BTreeMap;

use qfs_core::{Engine, Path, StatementSpec};
use qfs_parser::Statement;
use qfs_server::{EndpointDef, PolicyDef};

use crate::policy::{assert_read_only, PolicyError};
use crate::rewrite::{referenced_columns, source_paths};
use crate::Method;

/// A parsed route pattern: an ordered list of literal / param segments. Matches a concrete
/// request path positionally, extracting the named param values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePattern {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A fixed path segment that must match verbatim.
    Literal(String),
    /// A `:name` / `{name}` param segment that binds the concrete segment to `name`.
    Param(String),
}

impl RoutePattern {
    /// Parse a route string into a pattern. A segment of the form `:name` or `{name}` is a
    /// param; everything else is a literal. Leading/trailing slashes are normalised away.
    #[must_use]
    pub fn parse(route: &str) -> Self {
        let segments = route
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|seg| {
                if let Some(name) = seg.strip_prefix(':') {
                    Segment::Param(name.to_string())
                } else if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
                {
                    Segment::Param(inner.to_string())
                } else {
                    Segment::Literal(seg.to_string())
                }
            })
            .collect();
        Self { segments }
    }

    /// The declared param names, in route order.
    #[must_use]
    pub fn param_names(&self) -> Vec<String> {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Param(name) => Some(name.clone()),
                Segment::Literal(_) => None,
            })
            .collect()
    }

    /// Match a concrete request path against this pattern, returning the extracted path params
    /// (`name → concrete segment`) on a match, or `None` if the path does not match (different
    /// segment count or a literal mismatch).
    #[must_use]
    pub fn match_path(&self, path: &str) -> Option<BTreeMap<String, String>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() != self.segments.len() {
            return None;
        }
        let mut params = BTreeMap::new();
        for (seg, part) in self.segments.iter().zip(parts.iter()) {
            match seg {
                Segment::Literal(lit) => {
                    if lit != part {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    params.insert(name.clone(), (*part).to_string());
                }
            }
        }
        Some(params)
    }
}

/// One compiled, live route: the matched method + pattern and the rehydrated query AST + its
/// declared param names. The [`Router`] holds one per registered endpoint.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    /// The route's HTTP method.
    pub method: Method,
    /// The parsed route pattern.
    pub pattern: RoutePattern,
    /// The rehydrated, span-normalised query AST (NO re-parse at request time).
    pub query: Statement,
    /// The declared param names (route params; the bind validates against these).
    pub params: Vec<String>,
    /// The endpoint name (for tracing + diagnostics).
    pub name: String,
}

/// A route-compile failure (registration time).
#[derive(Debug, Clone, PartialEq)]
pub enum CompileError {
    /// The endpoint has no backing query (a declared-but-empty endpoint cannot serve).
    NoQuery {
        /// The endpoint name.
        name: String,
    },
    /// The stored query spec could not be rehydrated (corrupt config row) — sanitised.
    BadSpec {
        /// The endpoint name.
        name: String,
        /// A sanitised detail.
        detail: String,
    },
    /// The endpoint query failed the read-only-policy gate at registration.
    Policy(PolicyError),
    /// A declared route param name SHADOWS a column the query references — substituting the
    /// param would replace the wrong `Expr::Col` node and silently widen access (collapse a
    /// scope/filter predicate into a tautology). Refused at registration so the
    /// injection-safe / identical-plan guarantee is UNCONDITIONAL (the param-name convention is
    /// enforced, not merely documented). The fix is to rename the route param to an identifier
    /// distinct from every column the query reads.
    ParamShadowsColumn {
        /// The offending route-param name (also names the column it would shadow).
        param: String,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::NoQuery { name } => {
                write!(f, "endpoint `{name}` has no backing query")
            }
            CompileError::BadSpec { name, detail } => {
                write!(f, "endpoint `{name}` has a malformed query spec: {detail}")
            }
            CompileError::Policy(p) => write!(f, "{p}"),
            CompileError::ParamShadowsColumn { param } => write!(
                f,
                "route param `{param}` shadows a column the query references; substituting it \
                 would widen access. Rename the route param to an identifier distinct from \
                 every queried column"
            ),
        }
    }
}

impl std::error::Error for CompileError {}

/// Compile one [`EndpointDef`] into a [`CompiledRoute`], rehydrating its pre-parsed query and
/// running the **registration-time read-only policy gate**. The query body is the canonical
/// [`StatementSpec`] string stored by t31; it rehydrates via `from_canonical` (no re-parse).
/// A write-lowering query is REFUSED here (the plan-assertion acceptance) unless `policy`
/// grants it.
///
/// `engine` supplies the mounts the registration-time plan lowering resolves against, and
/// `policy` is the resolved [`PolicyDef`] the endpoint's `policy` handle names (if any).
///
/// # Errors
/// [`CompileError`] if the endpoint has no query, the spec is malformed, or the query lowers
/// to a write effect with no granting policy.
pub fn compile_endpoint(
    def: &EndpointDef,
    engine: &Engine,
    policy: Option<&PolicyDef>,
) -> Result<CompiledRoute, CompileError> {
    let canonical = def.query.as_str();
    if canonical.trim().is_empty() {
        return Err(CompileError::NoQuery {
            name: def.name.clone(),
        });
    }
    let spec =
        StatementSpec::from_canonical(canonical).map_err(|detail| CompileError::BadSpec {
            name: def.name.clone(),
            detail,
        })?;
    let query = spec.statement().clone();

    // Registration-time read-only policy gate (the plan-assertion acceptance). Build the
    // lowered plan from the query: a pure read lowers to `Plan::pure()` (no effects → passes);
    // a write lowers to an effect plan and is refused unless `policy` grants it.
    let plan = qfs_exec::build_plan(&query, engine).map_err(|e| {
        // A query that cannot even be planned at registration is treated as a bad spec
        // (sanitised) rather than silently registering an un-evaluable route.
        CompileError::BadSpec {
            name: def.name.clone(),
            detail: e.message,
        }
    })?;
    assert_read_only(&plan, policy).map_err(CompileError::Policy)?;

    let pattern = RoutePattern::parse(&def.route);
    let params = pattern.param_names();

    // Param-shadow gate (the t32 security fix): a route param whose name collides with a column
    // the query reads would make the typed-AST rewrite replace the WRONG `Expr::Col` node and
    // silently widen access. Refuse at registration so the injection-safe / identical-plan
    // guarantee is UNCONDITIONAL for every endpoint that registers (the distinct-name
    // convention is enforced, not merely documented).
    assert_no_param_shadows_column(&params, &query, engine)?;

    Ok(CompiledRoute {
        method: Method::parse(&def.method),
        pattern,
        query,
        params,
        name: def.name.clone(),
    })
}

/// Refuse registration if any declared route `params` name shadows a column the `query` reads
/// (the t32 access-widening fix). The check is **schema-precise where possible and fail-closed
/// otherwise**:
///
///   * For every `/driver/...` source the query reads, resolve the driver via `engine`'s
///     mounts and `describe` its REAL column names. A param whose name is one of those columns
///     is a genuine shadow → refuse (the precise access-widening case, e.g. param `id` in
///     `WHERE id = id` over a source with an `id` column).
///   * A param slot legitimately appears as an `Expr::Col(param)` (the bare-identifier param
///     convention), so membership in the referenced-column set alone is NOT a shadow. But a
///     param that is referenced as a column AND belongs to a source whose schema we could NOT
///     resolve is treated as UNVERIFIABLE → refuse (fail closed): we cannot prove the param is
///     not a real column of that late-bound source.
///
/// # Errors
/// [`CompileError::ParamShadowsColumn`] naming the first offending param.
fn assert_no_param_shadows_column(
    params: &[String],
    query: &Statement,
    engine: &Engine,
) -> Result<(), CompileError> {
    if params.is_empty() {
        return Ok(());
    }

    // Resolve the REAL column names of every readable source. `resolved_all` is false if any
    // source could not be resolved + described to a non-empty schema (→ fail-closed below).
    let mut schema_columns: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut resolved_all = true;
    let srcs = source_paths(query);
    if srcs.is_empty() {
        // No mount source at all (e.g. a pure VALUES relation): no real columns to shadow, but
        // we cannot verify against a schema, so fall through to the fail-closed referenced-set
        // check below.
        resolved_all = false;
    }
    for src in &srcs {
        match engine.mounts.resolve_path(src) {
            Some((driver, _rest)) => match driver.describe(&Path::new(src.clone())) {
                Ok(desc) if !desc.schema.columns.is_empty() => {
                    for col in &desc.schema.columns {
                        schema_columns.insert(col.name.clone());
                    }
                }
                // Late-bound / undescribable schema: columns unknown for this source.
                _ => resolved_all = false,
            },
            None => resolved_all = false,
        }
    }

    let referenced = referenced_columns(query);
    for param in params {
        // (a) Precise: the param IS a real data column of a resolved source.
        if schema_columns.contains(param) {
            return Err(CompileError::ParamShadowsColumn {
                param: param.clone(),
            });
        }
        // (b) Fail-closed: a source schema was unverifiable AND the param is referenced as a
        // column — we cannot prove it is not a real column, so refuse rather than risk widening.
        if !resolved_all && referenced.contains(param) {
            return Err(CompileError::ParamShadowsColumn {
                param: param.clone(),
            });
        }
    }
    Ok(())
}

/// The route table: an ordered list of compiled routes the [`crate::HttpBinding`] swaps
/// atomically. Matching is linear (the endpoint set is small); the FIRST method+path match
/// wins (deterministic registration order).
#[derive(Debug, Clone, Default)]
pub struct Router {
    routes: Vec<CompiledRoute>,
}

impl Router {
    /// An empty router (the boot starting point).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a router from a slice of compiled routes.
    #[must_use]
    pub fn from_routes(routes: Vec<CompiledRoute>) -> Self {
        Self { routes }
    }

    /// The number of live routes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the router has no routes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Resolve a request (method + path) to the matching compiled route + its extracted path
    /// params, or `None` if no route matches (→ 404).
    #[must_use]
    pub fn match_request(
        &self,
        method: &Method,
        path: &str,
    ) -> Option<(&CompiledRoute, BTreeMap<String, String>)> {
        for route in &self.routes {
            if &route.method == method {
                if let Some(params) = route.pattern.match_path(path) {
                    return Some((route, params));
                }
            }
        }
        None
    }
}
