//! The **closed-core server-binding DDL** desugar layer (RFD-0001 §3 frozen Server DDL,
//! §8 bindings, §6 effect-plan target). t31.
//!
//! This module is the *canonical* home of the five frozen `CREATE …` binding forms and the
//! one rule that makes the closed-core thesis hold for the server: each form is **pure
//! sugar** that desugars to exactly one `INSERT INTO /server/{endpoints,triggers,jobs,
//! views,webhooks}` effect-plan. It lives in `cfs-core` (not a driver) precisely because the
//! keywords are frozen and shared — a new backend adds zero variants here. `cfs-server`
//! consumes this layer; the `cfs-core → cfs-parser` edge (already wired for name resolution)
//! lets the desugar read the owned `cfs_parser` AST without any vendor leak.
//!
//! ## Grammar surface (zero new keywords)
//! The forms bind over the **existing t04 grammar** (which is the committed frozen keyword
//! set): the endpoint method+route and the webhook route both ride in the `ON '<…>'` operand,
//! `EVERY` carries the job interval, `AS`/`DO` carry the bodies. t31 introduces **no** new
//! closed-core keyword (no `AT`, no bare `<method> <route>` token) — it only adds the typed
//! DTO + desugar layer over what the lexer already freezes.
//!
//! ## Deferred bodies as fully-parsed serializable specs (the hard part, RFD §5/§8)
//! `AS <query>` / `DO <plan>` are **parsed and type-checked NOW** (a malformed binding is
//! rejected at `CREATE` time — important for AI per RFD §5) and stored as a [`StatementSpec`]
//! / [`PlanSpec`]: a *serializable representation of the parsed AST*, not raw source text and
//! not an AST `Debug` projection. The runtime rehydrates the spec via serde WITHOUT
//! re-parsing, so it can never hit a parse error at fire time. The spec is **data**
//! ([`PlanSpec`]), never a live `Plan` that could be committed by accident — embedding a
//! `DO <plan>` body must not execute it (the purity invariant, RFD §3).
//!
//! ## CREATE ≡ INSERT equivalence (closes the t30 body-storage gap, CO-t30-2/3)
//! Both a body-bearing `CREATE … DO <plan>` and its hand-written `INSERT INTO /server/…`
//! twin normalise the body into the **same** canonical [`StatementSpec`]: the CREATE form
//! parses its inline body; the INSERT form parses its `plan`/`query` **string column** into
//! the identical spec. The spec's serialized form is **span-normalised** (byte offsets
//! zeroed), so the two parse origins — which carry different source spans — converge to one
//! byte-identical canonical form. So the equivalence the t30 stopgap could not reach now
//! genuinely holds.

use serde::{Deserialize, Serialize};

use cfs_parser::{parse_statement, DdlKind, ParseError, ServerDdl, Statement};

use cfs_plan::{
    Affected, EffectKind, EffectNode, Plan, PlanBuilder, ServerNode, ServerWriteOp, Target, VfsPath,
};
use cfs_types::{Column, ColumnType, DriverId, Row, RowBatch, Schema, Value};

mod spec;
pub use spec::{normalize_spans, PlanSpec, StatementSpec};

#[cfg(test)]
mod tests;

/// The reserved server mount the bindings live under (RFD §8).
pub const SERVER_MOUNT: &str = "/server";

/// A typed seam for the t34 `POLICY` layer (RFD §10 least-privilege). Every binding decl
/// carries an optional, owned policy reference so t34 can attach a capability policy
/// **without a schema migration**. `None` until t34 wires it; stored as data, never a token.
pub type PolicyRef = Option<String>;

// ---------------------------------------------------------------------------
// Driver-agnostic value types (owned, no vendor leak)
// ---------------------------------------------------------------------------

/// An HTTP method for `CREATE ENDPOINT <method> <route>`. A closed, owned set — a new
/// backend adds none. `Other` keeps an unrecognised-but-syntactically-valid method as owned
/// text rather than rejecting (the route table, not the grammar, is the method authority).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// Any other method token, kept verbatim (uppercased).
    Other(String),
}

impl HttpMethod {
    /// Parse a method token (case-insensitive) into the closed set, else [`HttpMethod::Other`].
    #[must_use]
    pub fn parse(tok: &str) -> Self {
        match tok.trim().to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            other => Self::Other(other.to_string()),
        }
    }

    /// The canonical uppercase method text (the stored `method` column value).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Other(s) => s,
        }
    }
}

/// A route path for an endpoint / webhook, e.g. `/recent`. Owned text; the binding (E7)
/// interprets it, the grammar only carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route(pub String);

impl Route {
    /// The raw route text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A parsed `EVERY <interval>` cron interval (e.g. `5m`, `1h`). Stored as owned canonical
/// text; the t32 scheduler interprets the cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval(pub String);

impl Interval {
    /// The raw interval text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A parsed `ON <event>` trigger event reference (e.g. an `inbox` source-change tag, or a
/// webhook ref). Owned text; the t33 ingestion binding resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRef(pub String);

impl EventRef {
    /// The raw event text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Per-form decl DTOs (owned, vendor-free)
// ---------------------------------------------------------------------------

/// `CREATE ENDPOINT <name> ON '<method> /route' AS <query>` — an HTTP endpoint binding.
/// (The frozen grammar carries `<method> <route>` in the `ON` operand; t31 adds no `AT`/bare
/// method-route keywords beyond the frozen set.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointDecl {
    /// The handler name (the config row key).
    pub name: String,
    /// The HTTP method.
    pub method: HttpMethod,
    /// The served route.
    pub route: Route,
    /// The backing query (`AS <query>`), parsed + type-checked now, stored as a spec.
    /// `None` if the `AS` clause is omitted (a declared-but-empty endpoint, body filled
    /// later) — the body, when present, is rejected at CREATE time if it does not parse.
    pub query: Option<StatementSpec>,
    /// The t34 POLICY seam (RFD §10) — `None` until t34.
    pub policy_ref: PolicyRef,
}

/// `CREATE TRIGGER <name> ON <event> [WHERE <pred>] DO <plan>` — an event trigger binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDecl {
    /// The trigger name (the config row key).
    pub name: String,
    /// The event this trigger fires on.
    pub event: EventRef,
    /// The optional `WHERE <pred>` guard, stored as a spec (parsed now).
    pub predicate: Option<StatementSpec>,
    /// The effect-plan to run when the trigger fires (`DO <plan>`), parsed now, stored as a
    /// spec. `None` if the `DO` clause is omitted (a declared-but-empty trigger).
    pub plan: Option<PlanSpec>,
    /// The t34 POLICY seam.
    pub policy_ref: PolicyRef,
}

/// `CREATE JOB <name> EVERY <interval> DO <plan>` — a cron-job binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobDecl {
    /// The job name (the config row key).
    pub name: String,
    /// The cron interval.
    pub every: Interval,
    /// The effect-plan to run on each fire (`DO <plan>`), parsed now, stored as a spec.
    /// `None` if the `DO` clause is omitted (a declared-but-empty job).
    pub plan: Option<PlanSpec>,
    /// The t34 POLICY seam.
    pub policy_ref: PolicyRef,
}

/// `CREATE [MATERIALIZED] VIEW <path> AS <query>` — a view binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDecl {
    /// The view name (the config row key).
    pub name: String,
    /// The backing query (`AS <query>`), parsed now, stored as a spec. `None` if `AS` is
    /// omitted (a declared-but-empty view).
    pub query: Option<StatementSpec>,
    /// `true` for `MATERIALIZED VIEW`, `false` for a plain `VIEW`.
    pub materialized: bool,
    /// The t34 POLICY seam.
    pub policy_ref: PolicyRef,
}

/// `CREATE WEBHOOK <name> ON '<route>'` — an inbound webhook binding. (The frozen grammar
/// carries the route in the `ON` operand; t31 adds no `AT` keyword.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookDecl {
    /// The webhook name (the config row key).
    pub name: String,
    /// The inbound route.
    pub route: Route,
    /// The t34 POLICY seam.
    pub policy_ref: PolicyRef,
}

/// The five frozen server-binding DDL forms (RFD §3). A sum type, one variant per form —
/// closed; a new backend adds none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerBindingDdl {
    /// `CREATE ENDPOINT …`
    Endpoint(EndpointDecl),
    /// `CREATE TRIGGER …`
    Trigger(TriggerDecl),
    /// `CREATE JOB …`
    Job(JobDecl),
    /// `CREATE [MATERIALIZED] VIEW …`
    View(ViewDecl),
    /// `CREATE WEBHOOK …`
    Webhook(WebhookDecl),
}

impl ServerBindingDdl {
    /// The `/server/<kind>` collection this binding desugars into.
    #[must_use]
    pub fn node(&self) -> ServerNode {
        match self {
            Self::Endpoint(_) => ServerNode::Endpoints,
            Self::Trigger(_) => ServerNode::Triggers,
            Self::Job(_) => ServerNode::Jobs,
            Self::View(_) => ServerNode::Views,
            Self::Webhook(_) => ServerNode::Webhooks,
        }
    }

    /// The binding's name (the config row key).
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Endpoint(d) => &d.name,
            Self::Trigger(d) => &d.name,
            Self::Job(d) => &d.name,
            Self::View(d) => &d.name,
            Self::Webhook(d) => &d.name,
        }
    }
}

// ---------------------------------------------------------------------------
// Validation error
// ---------------------------------------------------------------------------

/// A structured, secret-free error from parsing/desugaring a server-binding DDL. Either a
/// wrapped [`ParseError`] (malformed body) or a binding-validation failure (unsupported
/// node, unknown column, malformed clause). No panic, AI-self-correctable (RFD §5).
#[derive(Debug, Clone, PartialEq)]
pub enum DdlError {
    /// A body (`AS <query>` / `DO <plan>` / `WHERE <pred>`) failed to parse.
    Parse(ParseError),
    /// A clause was malformed or violated the `/server/*` schema/capabilities.
    Validation {
        /// A machine-readable code (e.g. `UNKNOWN_COLUMN`, `MISSING_CLAUSE`).
        code: &'static str,
        /// A secret-free human message.
        message: String,
    },
}

impl DdlError {
    fn validation(code: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            code,
            message: message.into(),
        }
    }

    /// The stable machine-readable code (for the AI structured-error path).
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Parse(e) => e.code.as_str(),
            Self::Validation { code, .. } => code,
        }
    }
}

impl core::fmt::Display for DdlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::Validation { code, message } => write!(f, "[{code}] {message}"),
        }
    }
}

impl std::error::Error for DdlError {}

impl From<ParseError> for DdlError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

// ---------------------------------------------------------------------------
// Parse / validate a t04 `ServerDdl` into the structured binding DDL
// ---------------------------------------------------------------------------

/// Build a structured [`ServerBindingDdl`] from the t04-parsed [`ServerDdl`] shape,
/// type-checking and span-normalising the deferred bodies **now**.
///
/// The t04 grammar already parses `CREATE …` into a permissive [`ServerDdl`] (clauses in any
/// order, bodies as `Statement`). This is the *validation + normalisation* step: it checks
/// each form has the clauses it requires, wraps the bodies as span-normalised specs, and
/// rejects an unknown `CREATE` subkeyword (e.g. `POLICY`, deferred to t34) with a structured
/// error rather than a panic.
///
/// # Errors
/// [`DdlError::Validation`] for a missing/extra clause or an unsupported subkeyword.
pub fn from_server_ddl(ddl: &ServerDdl) -> Result<ServerBindingDdl, DdlError> {
    let name = ddl.name.clone();
    match ddl.kind {
        DdlKind::Endpoint => {
            let (method, route) = split_method_route(ddl.on.as_deref().ok_or_else(|| {
                DdlError::validation(
                    "MISSING_CLAUSE",
                    "CREATE ENDPOINT requires `ON '<method> /route'`",
                )
            })?)?;
            Ok(ServerBindingDdl::Endpoint(EndpointDecl {
                name,
                method,
                route,
                query: ddl
                    .as_query
                    .as_deref()
                    .map(StatementSpec::from_statement_ref),
                policy_ref: None,
            }))
        }
        DdlKind::Trigger => {
            let event = EventRef(ddl.on.clone().ok_or_else(|| {
                DdlError::validation("MISSING_CLAUSE", "CREATE TRIGGER requires `ON <event>`")
            })?);
            Ok(ServerBindingDdl::Trigger(TriggerDecl {
                name,
                event,
                // The optional `WHERE <pred>` is carried in t04 as part of the DO body shape;
                // a standalone predicate clause is not separately surfaced by t04, so the
                // predicate seam stays `None` here (parsed-now when t04 surfaces it).
                predicate: None,
                plan: ddl.do_plan.as_deref().map(PlanSpec::from_statement_ref),
                policy_ref: None,
            }))
        }
        DdlKind::Job => {
            let every = Interval(ddl.every.clone().ok_or_else(|| {
                DdlError::validation("MISSING_CLAUSE", "CREATE JOB requires `EVERY <interval>`")
            })?);
            Ok(ServerBindingDdl::Job(JobDecl {
                name,
                every,
                plan: ddl.do_plan.as_deref().map(PlanSpec::from_statement_ref),
                policy_ref: None,
            }))
        }
        DdlKind::View | DdlKind::MaterializedView => Ok(ServerBindingDdl::View(ViewDecl {
            name,
            query: ddl
                .as_query
                .as_deref()
                .map(StatementSpec::from_statement_ref),
            materialized: matches!(ddl.kind, DdlKind::MaterializedView),
            policy_ref: None,
        })),
        DdlKind::Webhook => {
            let route = Route(ddl.on.clone().ok_or_else(|| {
                DdlError::validation("MISSING_CLAUSE", "CREATE WEBHOOK requires `ON '<route>'`")
            })?);
            Ok(ServerBindingDdl::Webhook(WebhookDecl {
                name,
                route,
                policy_ref: None,
            }))
        }
        DdlKind::Policy => Err(DdlError::validation(
            "UNSUPPORTED_DDL",
            "CREATE POLICY is deferred to t34 (capability gating); not a t31 binding form",
        )),
    }
}

/// Parse a source string straight into a structured [`ServerBindingDdl`] (the AI/CLI entry
/// point). Parses the statement, requires it to be a `CREATE …` DDL, then validates it.
///
/// # Errors
/// [`DdlError`] on a parse failure or a non-DDL statement / unsupported subkeyword.
pub fn parse_server_binding_ddl(src: &str) -> Result<ServerBindingDdl, DdlError> {
    let stmt = parse_statement(src)?;
    match stmt {
        Statement::Ddl(ddl) => from_server_ddl(&ddl),
        _ => Err(DdlError::validation(
            "NOT_DDL",
            "expected a CREATE binding DDL statement",
        )),
    }
}

/// Split an endpoint `<method> <route>` token (t04 carries it as the ON operand). A bare
/// route (no method) yields a `GET` default.
fn split_method_route(on: &str) -> Result<(HttpMethod, Route), DdlError> {
    let trimmed = on.trim();
    if trimmed.is_empty() {
        return Err(DdlError::validation(
            "MISSING_CLAUSE",
            "CREATE ENDPOINT requires a route",
        ));
    }
    match trimmed.split_once(char::is_whitespace) {
        Some((method, route)) => Ok((HttpMethod::parse(method), Route(route.trim().to_string()))),
        // A bare `/route` defaults to GET.
        None => Ok((HttpMethod::Get, Route(trimmed.to_string()))),
    }
}

// ---------------------------------------------------------------------------
// The canonical config row + desugar to a /server INSERT plan
// ---------------------------------------------------------------------------

/// The canonical normalised config row both desugar paths (CREATE sugar / INSERT twin)
/// produce — a name→value map keyed/sorted by name so the resulting [`RowBatch`] is
/// deterministic and the CREATE-vs-INSERT equivalence is exact (byte-identical plan nodes).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigRow {
    fields: std::collections::BTreeMap<String, Value>,
}

impl ConfigRow {
    /// Set a typed column value.
    pub fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_string(), value);
    }

    /// Set a text column value.
    pub fn set_text(&mut self, key: &str, text: impl Into<String>) {
        self.fields
            .insert(key.to_string(), Value::Text(text.into()));
    }

    /// Read a column value, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// The idempotency verb for a `CREATE …` binding (RFD §6). **`UPSERT`-by-name**: re-applying
/// a `config.cfs` (a boot replay or retry) converges to the same `ServerState` rather than
/// failing on a duplicate name. This is coherent with t30, which already chose `UPSERT` so
/// re-applying a config file is a no-op. A future strict `INSERT`-with-duplicate-error path
/// remains available via the explicit `INSERT INTO /server/…` form (the apply layer rejects a
/// duplicate `INSERT`), so a caller who wants fail-on-duplicate uses that verb directly.
pub const CREATE_WRITE_OP: ServerWriteOp = ServerWriteOp::Upsert;

/// Desugar a structured binding into its canonical [`ConfigRow`] (the seam both the CREATE
/// sugar and its INSERT twin normalise into). The deferred body is stored as its canonical
/// span-normalised spec **string** so the INSERT twin — which parses the same source from its
/// string column — produces a byte-identical row (CREATE ≡ INSERT).
#[must_use]
pub fn binding_config_row(ddl: &ServerBindingDdl) -> ConfigRow {
    let mut row = ConfigRow::default();
    row.set_text("name", ddl.name());
    match ddl {
        ServerBindingDdl::Endpoint(d) => {
            row.set_text("method", d.method.as_str());
            row.set_text("route", d.route.as_str());
            row.set_text(
                "query",
                canonical_or_empty(d.query.as_ref().map(StatementSpec::canonical)),
            );
        }
        ServerBindingDdl::Trigger(d) => {
            row.set_text("on", d.event.as_str());
            row.set_text(
                "plan",
                canonical_or_empty(d.plan.as_ref().map(PlanSpec::canonical)),
            );
        }
        ServerBindingDdl::Job(d) => {
            row.set_text("every", d.every.as_str());
            row.set_text(
                "plan",
                canonical_or_empty(d.plan.as_ref().map(PlanSpec::canonical)),
            );
        }
        ServerBindingDdl::View(d) => {
            row.set_text(
                "query",
                canonical_or_empty(d.query.as_ref().map(StatementSpec::canonical)),
            );
            row.set("materialized", Value::Bool(d.materialized));
        }
        ServerBindingDdl::Webhook(d) => {
            row.set_text("route", d.route.as_str());
        }
    }
    row
}

/// The canonical body string, or empty when the body clause is omitted (a declared-but-empty
/// binding — coherent with the body-less INSERT twin storing an empty `plan`/`query`).
fn canonical_or_empty(canonical: Option<String>) -> String {
    canonical.unwrap_or_default()
}

/// The driver id `/server` writes route to (the mount stripped of its leading `/`).
#[must_use]
fn server_driver_id() -> DriverId {
    DriverId::new(SERVER_MOUNT.trim_start_matches('/'))
}

/// Build the canonical [`RowBatch`] for `node` from a [`ConfigRow`], emitting columns in the
/// node's schema order (the single source of truth shared with `DESCRIBE`), filling absent
/// fields with `Null` and **rejecting an unknown column** (a field with no schema slot is a
/// malformed config write — RFD §6 honest typing).
///
/// # Errors
/// [`DdlError::Validation`] (`UNKNOWN_COLUMN`) if the row carries a field the node's schema
/// does not declare.
pub fn config_row_batch(node: ServerNode, row: &ConfigRow) -> Result<RowBatch, DdlError> {
    let schema = server_node_schema(node);
    let known: std::collections::BTreeSet<&str> =
        schema.columns.iter().map(|c| c.name.as_str()).collect();
    for field in row.fields.keys() {
        if !known.contains(field.as_str()) {
            return Err(DdlError::validation(
                "UNKNOWN_COLUMN",
                format!("/server/{} has no column `{field}`", node.segment()),
            ));
        }
    }
    let mut cols = Vec::with_capacity(schema.columns.len());
    let mut values = Vec::with_capacity(schema.columns.len());
    for col in &schema.columns {
        let v = row.fields.get(&col.name).cloned().unwrap_or(Value::Null);
        cols.push(Column::new(col.name.clone(), col.ty.clone(), col.nullable));
        values.push(v);
    }
    Ok(RowBatch::new(Schema::new(cols), vec![Row::new(values)]))
}

/// Assemble a one-node `/server` write [`Plan`] (the pure effect node). Building this mutates
/// nothing — the COMMIT-time apply is the only impure op. `affected = Exact(1)` (one row).
#[must_use]
pub fn server_write_plan(node: ServerNode, op: ServerWriteOp, args: RowBatch) -> Plan {
    let mut builder = PlanBuilder::new();
    let target = Target::new(server_driver_id(), VfsPath::new(node.path()));
    let effect = EffectNode::new(
        builder.next_id(),
        EffectKind::ServerConfigWrite { node, op },
        target,
    )
    .with_args(args)
    .with_affected(Affected::Exact(1));
    builder.push(effect);
    builder.build()
}

/// Desugar a server binding into exactly one `INSERT INTO /server/<kind>` effect-plan
/// (the canonical closed-core rule, RFD §6/§8). Pure: constructs a [`Plan`], runs no I/O.
///
/// # Errors
/// [`DdlError`] if the desugared row violates the `/server/*` schema.
pub fn desugar_to_insert(ddl: &ServerBindingDdl) -> Result<Plan, DdlError> {
    let node = ddl.node();
    let row = binding_config_row(ddl);
    let args = config_row_batch(node, &row)?;
    Ok(server_write_plan(node, CREATE_WRITE_OP, args))
}

/// The trait the ticket names: desugar a binding to its single `/server` INSERT plan.
pub trait DesugarToInsert {
    /// Desugar to one `INSERT INTO /server/<kind>` effect-plan.
    ///
    /// # Errors
    /// [`DdlError`] on a schema violation.
    fn desugar(&self) -> Result<Plan, DdlError>;
}

impl DesugarToInsert for ServerBindingDdl {
    fn desugar(&self) -> Result<Plan, DdlError> {
        desugar_to_insert(self)
    }
}

// ---------------------------------------------------------------------------
// The canonical /server/* schema (the single source of truth, shared with DESCRIBE)
// ---------------------------------------------------------------------------

/// The typed [`Schema`] of a `/server/<node>` config node — the **canonical** source of
/// truth `DESCRIBE /server/<node>` and the desugar both read. Pure data; no live backend.
///
/// (`cfs-server` re-exports this so the driver's `DESCRIBE` and the DDL desugar cannot drift
/// — the schema lives in closed core with the frozen DDL.)
#[must_use]
pub fn server_node_schema(node: ServerNode) -> Schema {
    let col = |name: &str, ty: ColumnType, nullable: bool| Column::new(name, ty, nullable);
    match node {
        ServerNode::Endpoints => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("method", ColumnType::Text, true),
            col("route", ColumnType::Text, true),
            col("query", ColumnType::Text, true),
        ]),
        ServerNode::Triggers => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("on", ColumnType::Text, true),
            col("plan", ColumnType::Text, true),
        ]),
        ServerNode::Jobs => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("every", ColumnType::Text, true),
            col("plan", ColumnType::Text, true),
            col("last_run", ColumnType::Timestamp, true),
        ]),
        ServerNode::Views => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("query", ColumnType::Text, true),
            col("materialized", ColumnType::Bool, false),
        ]),
        ServerNode::Policies => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("handler", ColumnType::Text, true),
            col("allow", ColumnType::Array(Box::new(ColumnType::Text)), true),
        ]),
        ServerNode::Webhooks => Schema::new(vec![
            col("name", ColumnType::Text, false),
            col("route", ColumnType::Text, true),
        ]),
    }
}
