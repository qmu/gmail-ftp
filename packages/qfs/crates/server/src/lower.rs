//! Lower a server-config statement to a **pure** `/server` write plan (RFD-0001 §6/§8).
//!
//! This is the single desugaring seam that makes the closed-core thesis hold for the
//! server: a `CREATE JOB … EVERY … DO …` and the equivalent `INSERT INTO /server/jobs …`
//! lower to **identical** [`EffectKind::ServerConfigWrite`](qfs_core::EffectKind::ServerConfigWrite)
//! plan nodes. Both paths normalise into the same canonical named-field row and build the
//! same [`qfs_core::RowBatch`] (columns in the node's schema order), so the frozen `CREATE …`
//! DDL is provably **sugar** over the `/server` write (acceptance: golden plan equivalence).
//!
//! ## The canonical desugar lives in `qfs-core` (t31)
//! The five frozen `CREATE …` binding forms and the desugar to `INSERT INTO /server/*` are
//! owned by closed core ([`qfs_core::ddl::server`]) because the keywords are frozen and
//! shared. This module is the **server-side adapter**: it routes a parsed `Statement` to the
//! core desugar (`CREATE`) or normalises an explicit `INSERT INTO /server/…` into the same
//! canonical row (the INSERT twin), so the two converge on one plan node.
//!
//! ## Deferred bodies as fully-parsed specs (t31, closes the t30 gap CO-t30-2/3)
//! `AS <query>` / `DO <plan>` are stored as a canonical, span-normalised
//! [`qfs_core::StatementSpec`]/[`qfs_core::PlanSpec`] — a *parsed* AST serialized to a stable
//! form, NOT the AST `Debug` projection t30 used as a stopgap. The `/server/*` `plan`/`query`
//! STRING column the INSERT twin supplies is **parsed into the same canonical spec**, so a
//! body-bearing `CREATE … DO <plan>` and its `INSERT INTO /server/jobs` twin now store the
//! IDENTICAL body — genuine CREATE ≡ INSERT equivalence.
//!
//! Lowering is **pure**: it constructs a [`qfs_core::Plan`] and mutates **nothing** — the
//! interpreter mutates [`ServerState`](crate::ServerState) only at `COMMIT`.

use qfs_core::{
    binding_config_row, config_row_batch, from_server_ddl, server_write_plan, ConfigRow, Plan,
    PlanSpec, ServerNode, ServerWriteOp, StatementSpec, Value, CREATE_WRITE_OP,
};
use qfs_parser::{
    parse_statement, DdlKind, EffectBody, EffectStmt, EffectVerb, Expr, Literal, ServerDdl,
    Statement, Values,
};

/// Lower a parsed [`Statement`] to a `/server` config write plan, or `Ok(None)` if the
/// statement is not a server-config statement (the caller decides whether that is an error).
///
/// Handles both forms:
/// - `Statement::Ddl(CREATE …)` — the frozen DDL **sugar** (desugars through `qfs-core`);
/// - `Statement::Effect(INSERT/UPSERT/UPDATE/REMOVE INTO /server/<node> …)` — the explicit
///   write the sugar desugars to.
///
/// Both normalise into the same canonical row and so produce **identical** plan nodes.
///
/// # Errors
/// A secret-free detail string if the statement targets `/server` but is malformed (bad
/// node segment, unsupported verb, non-literal values, or an unparseable embedded body).
pub fn lower_statement(stmt: &Statement) -> Result<Option<Plan>, String> {
    match stmt {
        Statement::Ddl(ddl) => Ok(Some(lower_ddl(ddl)?)),
        Statement::Effect(effect) if targets_server(effect) => Ok(Some(lower_effect(effect)?)),
        // A `PREVIEW`/`COMMIT` wrapper around a server statement unwraps transparently.
        Statement::Plan(wrap) => lower_statement(&wrap.inner),
        _ => Ok(None),
    }
}

/// Whether an effect statement targets the `/server/...` mount.
fn targets_server(effect: &EffectStmt) -> bool {
    effect
        .target
        .segments
        .first()
        .is_some_and(|s| s.name == "server")
}

/// Lower a `CREATE … DDL` to its `/server` write plan via the **canonical core desugar**
/// ([`qfs_core::from_server_ddl`] → [`qfs_core::desugar_to_insert`]). The deferred bodies are
/// stored as canonical span-normalised specs by core; this is the single source of truth the
/// INSERT twin reproduces.
fn lower_ddl(ddl: &ServerDdl) -> Result<Plan, String> {
    // POLICY is NOT a t31 binding form (it is the t34 capability-gating concern). t30 already
    // stores a POLICY row; the t31 core binding layer covers only the five frozen forms. So
    // POLICY is desugared here in the server adapter, not through the core binding layer.
    if matches!(ddl.kind, DdlKind::Policy) {
        return lower_policy(ddl);
    }
    let binding = from_server_ddl(ddl).map_err(|e| e.to_string())?;
    let node = binding.node();
    let row = binding_config_row(&binding);
    let args = config_row_batch(node, &row).map_err(|e| e.to_string())?;
    // CREATE is a declarative "make this exist" — UPSERT-by-name (the boot/replay-safe verb,
    // RFD §6 idempotency), coherent with t30.
    Ok(server_write_plan(node, CREATE_WRITE_OP, args))
}

/// Desugar `CREATE POLICY <name> ALLOW … DENY …` to a `/server/policies` UPSERT (t35). The
/// `ALLOW`/`DENY` rules become the canonical rule strings in the `allow` array (so the CREATE
/// sugar and an `INSERT INTO /server/policies` twin store the IDENTICAL rows, and rehydrate to
/// an EQUAL `Policy` — the acceptance round-trip). The handler rides in the `ON` operand.
fn lower_policy(ddl: &ServerDdl) -> Result<Plan, String> {
    // Build the owned Policy from the parsed rules, then render the canonical rule strings.
    let policy = crate::policy::policy_from_ddl(ddl)?;
    let rule_strings = crate::policy::policy_to_rule_strings(&policy);

    let mut row = ConfigRow::default();
    row.set_text("name", ddl.name.clone());
    row.set_text("handler", ddl.on.clone().unwrap_or_default());
    row.set(
        "allow",
        Value::Array(rule_strings.into_iter().map(Value::Text).collect()),
    );
    // config_row_batch is total for a schema-valid row; policies has name/handler/allow.
    let args = config_row_batch(ServerNode::Policies, &row).unwrap_or_else(|_| {
        // Unreachable: the row only sets declared columns. Build an empty-row fallback.
        config_row_batch(ServerNode::Policies, &ConfigRow::default()).unwrap_or_else(|_| {
            qfs_core::RowBatch::new(qfs_core::Schema::new(Vec::new()), Vec::new())
        })
    });
    Ok(server_write_plan(
        ServerNode::Policies,
        CREATE_WRITE_OP,
        args,
    ))
}

/// Lower an explicit `INSERT/UPSERT/UPDATE/REMOVE INTO /server/<node> …` to the canonical
/// config write. The `/server/<node>` segment selects the collection; `VALUES (cols)(vals)`
/// (or `SET`) carries the named fields. The body-bearing columns (`plan`/`query`) are
/// **parsed into the same canonical [`PlanSpec`]/[`StatementSpec`]** the CREATE form stores,
/// so the INSERT twin and the CREATE sugar converge on one byte-identical body (t31).
fn lower_effect(effect: &EffectStmt) -> Result<Plan, String> {
    let segments: Vec<&str> = effect
        .target
        .segments
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let node_seg = segments
        .get(1)
        .ok_or_else(|| "/server write needs a collection, e.g. /server/jobs".to_string())?;
    let node = ServerNode::from_segment(node_seg)
        .ok_or_else(|| format!("unknown /server collection `{node_seg}`"))?;

    let op = match effect.verb {
        EffectVerb::Insert => ServerWriteOp::Insert,
        EffectVerb::Upsert => ServerWriteOp::Upsert,
        EffectVerb::Update => ServerWriteOp::Update,
        EffectVerb::Remove => ServerWriteOp::Remove,
    };

    let mut row = ConfigRow::default();
    // A trailing path segment after the collection names the row (e.g.
    // `REMOVE /server/jobs/nightly`); else the row name comes from a `name` column.
    if let Some(name_seg) = segments.get(2) {
        row.set_text("name", (*name_seg).to_string());
    }

    match &effect.body {
        EffectBody::Values(values) => {
            collect_values_row(node, values, &mut row)?;
        }
        EffectBody::SetWhere { set, .. } => {
            for assignment in set {
                let v = literal_value(&assignment.value)?;
                row.set(
                    &assignment.name,
                    normalize_body_column(node, &assignment.name, v),
                );
            }
        }
        EffectBody::Pipeline(_) => {
            return Err("/server writes take VALUES / SET, not a sub-pipeline".to_string());
        }
    }

    let args = config_row_batch(node, &row).map_err(|e| e.to_string())?;
    Ok(server_write_plan(node, op, args))
}

/// Collect a single-row `VALUES (col, …)(val, …)` into the canonical [`ConfigRow`]. Only a
/// single literal row is supported (a config write names one row); explicit column names are
/// required so the mapping to named fields is unambiguous.
fn collect_values_row(
    node: ServerNode,
    values: &Values,
    row: &mut ConfigRow,
) -> Result<(), String> {
    let columns = values.columns.as_ref().ok_or_else(|| {
        "/server INSERT requires explicit column names, e.g. VALUES (name, every) (...)".to_string()
    })?;
    let first = values
        .rows
        .first()
        .ok_or_else(|| "/server INSERT requires one VALUES row".to_string())?;
    if columns.len() != first.len() {
        return Err(format!(
            "/server INSERT column/value count mismatch ({} cols, {} vals)",
            columns.len(),
            first.len()
        ));
    }
    for (col, expr) in columns.iter().zip(first) {
        let v = literal_value(expr)?;
        row.set(col, normalize_body_column(node, col, v));
    }
    Ok(())
}

/// Normalise a `/server` column value: a body-bearing column (`plan`/`query`) carrying a
/// non-empty source STRING is **parsed into the canonical span-normalised spec** the CREATE
/// form stores, so the INSERT twin and the CREATE sugar converge on one byte-identical body
/// (t31 CREATE ≡ INSERT). All other columns (and an empty body) pass through unchanged.
///
/// Equivalence holds whenever the INSERT supplies genuine qfs source (the realistic
/// boot-replay case): the body parses to the same spec the CREATE form built. A body that is
/// **not** parseable qfs source (a placeholder, an opaque marker) is kept **verbatim** rather
/// than rejected — an explicit `INSERT INTO /server/…` may legitimately carry a non-qfs
/// marker string, and rejecting it would break the explicit-write path. So this never errors.
fn normalize_body_column(node: ServerNode, col: &str, value: Value) -> Value {
    if !is_body_column(node, col) {
        return value;
    }
    match value {
        // An empty body stays empty (the body-less CREATE stores an empty string too).
        Value::Text(s) if s.is_empty() => Value::Text(s),
        Value::Text(src) => match parse_statement(&src) {
            // `plan` columns are effect-plan bodies (PlanSpec); `query` columns are queries
            // (StatementSpec). Both canonicalise the same way (PlanSpec wraps StatementSpec),
            // so the stored string is identical to the CREATE form's.
            Ok(stmt) if col == "plan" => Value::Text(PlanSpec::from_statement(stmt).canonical()),
            Ok(stmt) => Value::Text(StatementSpec::from_statement(stmt).canonical()),
            // Not parseable qfs source: keep the literal verbatim (an opaque marker).
            Err(_) => Value::Text(src),
        },
        other => other,
    }
}

/// Whether `col` on `node` holds a deferred body (an `AS <query>` / `DO <plan>` spec).
fn is_body_column(node: ServerNode, col: &str) -> bool {
    matches!(
        (node, col),
        (ServerNode::Endpoints | ServerNode::Views, "query")
            | (ServerNode::Triggers | ServerNode::Jobs, "plan")
            // t34 (CO-t31-4): the trigger `predicate` column holds a query-style StatementSpec
            // (the `WHERE <pred>` wrapped over an empty VALUES source). An INSERT twin supplying
            // the same parseable body canonicalises to the byte-identical stored spec.
            | (ServerNode::Triggers, "predicate")
    )
}

/// Extract a [`Value`] from a literal expression. Server-config writes carry only literals
/// (names, routes, intervals, plan source text) — a non-literal is a malformed config write.
fn literal_value(expr: &Expr) -> Result<Value, String> {
    match expr {
        Expr::Lit(lit) => Ok(match lit {
            Literal::Str(s) => Value::Text(s.clone()),
            Literal::Int(n) => Value::Int(*n),
            Literal::Float(f) => Value::Float(*f),
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Null => Value::Null,
            Literal::Size { value, unit } => Value::Text(format!("{value} {unit}")),
            Literal::Typed { raw, .. } => Value::Text(raw.clone()),
        }),
        other => Err(format!(
            "/server config values must be literals, got {}",
            expr_kind(other)
        )),
    }
}

/// A short, secret-free label for a non-literal expression (for the structured error).
fn expr_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Lit(_) => "literal",
        Expr::Col(_) => "column reference",
        Expr::Path(_) => "path",
        Expr::Fn(_) => "function call",
        Expr::Binary { .. } => "binary op",
        Expr::Unary { .. } => "unary op",
        Expr::In { .. } => "IN",
        Expr::Between { .. } => "BETWEEN",
        Expr::Like { .. } => "LIKE",
        Expr::AnyOp { .. } => "ANY",
    }
}
