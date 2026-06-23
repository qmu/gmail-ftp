//! `emit` — the per-dialect **native-SQL emitter** (RFD-0001 §5/§6, ticket steps 6/10). It
//! renders a [`SelectPlan`] (compiled read) and a [`DmlOp`] (lowered write) into `(sql, params)`
//! where `params: Vec<Param>` carries **every value as a bound parameter** and the `sql` string
//! carries **only** identifiers and placeholders. No value is ever string-formatted into the SQL
//! — the headline injection-safety + correctness invariant of this driver (ticket "always bind
//! params"). A literal like `'; DROP TABLE t; --` therefore lands as a *bound value*, never as
//! executable text.
//!
//! Identifiers (table / schema / column) are dialect-quoted via [`Dialect::quote_ident`]; the
//! compiler/lowering has already validated every identifier against the catalog, so an unknown
//! column cannot reach the emitter. Placeholders are dialect-specific (`$n` for postgres, `?` for
//! mysql/sqlite) and are assigned in lockstep with `params` so the two never drift.

use cfs_types::{CmpOp, Literal, Value};

use crate::dialect::Dialect;

/// A bound parameter value — an owned scalar mirroring the cfs scalar [`Value`]s the backend
/// binds positionally. Carrying it as a typed enum (rather than a stringified literal) keeps the
/// value out of the SQL text and lets each backend bind it with the right type. Secret-free in the
/// sense that it is *query data*, never a credential; it is **not** rendered into any log line.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// A SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// UTF-8 text (the `'; DROP TABLE`-style injection attempt lands here, inert, as data).
    Text(String),
    /// Opaque bytes.
    Bytes(Vec<u8>),
}

impl Param {
    /// Lower a typed predicate [`Literal`] into a bound [`Param`].
    #[must_use]
    pub fn from_literal(lit: &Literal) -> Self {
        match lit {
            Literal::Null => Param::Null,
            Literal::Bool(b) => Param::Bool(*b),
            Literal::Int(n) => Param::Int(*n),
            Literal::Float(f) => Param::Float(*f),
            Literal::Text(t) => Param::Text(t.clone()),
        }
    }

    /// Lower a runtime row [`Value`] into a bound [`Param`] for a DML statement. Non-scalar
    /// values (`Struct`/`Array`/`Json`) are carried as their JSON text form so they still bind as
    /// data (never interpolated); `Timestamp` binds as its integer.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Param::Null,
            Value::Bool(b) => Param::Bool(*b),
            Value::Int(n) | Value::Timestamp(n) => Param::Int(*n),
            Value::Float(f) => Param::Float(*f),
            Value::Text(t) => Param::Text(t.clone()),
            Value::Bytes(b) => Param::Bytes(b.clone()),
            // Irregular / future variants (`Struct`/`Array`/`Json`, and any new `#[non_exhaustive]`
            // value) bind as their JSON text form — still a single bound parameter, never
            // interpolated. A serialization failure is impossible for these owned shapes, but we
            // fall back to an empty string rather than panicking (lib code is panic-free).
            other => Param::Text(serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

/// A compiled comparison leaf in a rendered `WHERE` — the column, the operator, and the param
/// index the value was bound at. Produced by the compiler, consumed by `render_where`.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlPredicate {
    /// `<col> <op> ?` — a bound comparison.
    Cmp {
        /// The (validated) column name.
        col: String,
        /// The comparison operator.
        op: CmpOp,
        /// The 0-based index into the plan's `params` of the bound value.
        param: usize,
    },
    /// `<col> IN (?, ?, ...)` — a bound membership test.
    InList {
        /// The column name.
        col: String,
        /// The 0-based param indices of the bound candidate values (in order).
        params: Vec<usize>,
    },
    /// `<col> BETWEEN ? AND ?` — a bound range.
    Between {
        /// The column name.
        col: String,
        /// The 0-based param index of the lower bound.
        low: usize,
        /// The 0-based param index of the upper bound.
        high: usize,
    },
    /// `<lhs> AND <rhs>` — a conjunction of compiled leaves.
    And(Box<SqlPredicate>, Box<SqlPredicate>),
}

/// One ORDER BY term: a (validated) column and direction.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderTerm {
    /// The column name.
    pub col: String,
    /// Descending if true.
    pub desc: bool,
}

/// A compiled SELECT — everything `render_select` needs plus the bound `params` (in placeholder
/// order). Built by [`crate::compile`].
#[derive(Debug, Clone, PartialEq)]
pub struct SelectPlan {
    /// The (empty for default) schema and the table name.
    pub schema: String,
    /// The table name.
    pub table: String,
    /// The projected columns, in SELECT order (empty ⇒ `SELECT *`).
    pub projection: Vec<String>,
    /// The compiled WHERE, if any.
    pub where_: Option<SqlPredicate>,
    /// The ORDER BY terms, in order.
    pub order_by: Vec<OrderTerm>,
    /// The LIMIT, if any.
    pub limit: Option<i64>,
    /// The bound parameters, indexed by the `param` fields above (in WHERE-traversal order).
    pub params: Vec<Param>,
}

/// Render a [`SelectPlan`] into `(sql, params)` for `dialect`. Identifiers are quoted; every value
/// is a placeholder; `params` is returned in placeholder order so the backend binds them
/// positionally. No value text appears in the SQL.
#[must_use]
pub fn render_select(dialect: Dialect, plan: &SelectPlan) -> (String, Vec<Param>) {
    let cols = if plan.projection.is_empty() {
        "*".to_string()
    } else {
        plan.projection
            .iter()
            .map(|c| dialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let table = dialect.quote_qualified(&plan.schema, &plan.table);
    let mut sql = format!("SELECT {cols} FROM {table}");

    // A monotonically increasing placeholder counter so `$n`/`?` and `params` stay in lockstep.
    let mut next_ph = 1usize;
    if let Some(pred) = &plan.where_ {
        sql.push_str(" WHERE ");
        sql.push_str(&render_where(dialect, pred, &mut next_ph));
    }
    if !plan.order_by.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(
            &plan
                .order_by
                .iter()
                .map(|t| {
                    let dir = if t.desc { " DESC" } else { " ASC" };
                    format!("{}{dir}", dialect.quote_ident(&t.col))
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(limit) = plan.limit {
        // LIMIT carries an integer literal; it is structural (not user value-bearing) and is
        // rendered as a non-negative integer constant after a checked cast — never a string from
        // user input. A negative limit is clamped to 0 (no rows) rather than emitting bad SQL.
        let n = limit.max(0);
        sql.push_str(&format!(" LIMIT {n}"));
    }

    (sql, params_in_where_order(plan))
}

/// Render a compiled `WHERE` predicate to text, emitting a placeholder per bound value via the
/// shared `next_ph` counter (so the placeholder order matches the params vector).
fn render_where(dialect: Dialect, pred: &SqlPredicate, next_ph: &mut usize) -> String {
    match pred {
        SqlPredicate::Cmp { col, op, .. } => {
            let ph = dialect.placeholder(*next_ph);
            *next_ph += 1;
            format!("{} {} {ph}", dialect.quote_ident(col), cmp_op_sql(*op))
        }
        SqlPredicate::InList { col, params } => {
            let placeholders = params
                .iter()
                .map(|_| {
                    let ph = dialect.placeholder(*next_ph);
                    *next_ph += 1;
                    ph
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} IN ({placeholders})", dialect.quote_ident(col))
        }
        SqlPredicate::Between { col, .. } => {
            let low = dialect.placeholder(*next_ph);
            *next_ph += 1;
            let high = dialect.placeholder(*next_ph);
            *next_ph += 1;
            format!("{} BETWEEN {low} AND {high}", dialect.quote_ident(col))
        }
        SqlPredicate::And(a, b) => {
            let left = render_where(dialect, a, next_ph);
            let right = render_where(dialect, b, next_ph);
            format!("({left}) AND ({right})")
        }
    }
}

/// Collect the bound params in the order the placeholders are emitted (the same left-to-right
/// traversal `render_where` performs), so positional binding is correct. The compiler already
/// stored each leaf's value indices into `plan.params`; this re-orders them to placeholder order.
fn params_in_where_order(plan: &SelectPlan) -> Vec<Param> {
    let mut ordered = Vec::with_capacity(plan.params.len());
    if let Some(pred) = &plan.where_ {
        collect_params(pred, &plan.params, &mut ordered);
    }
    ordered
}

/// Walk a compiled predicate in placeholder order, pushing each referenced param.
fn collect_params(pred: &SqlPredicate, source: &[Param], out: &mut Vec<Param>) {
    match pred {
        SqlPredicate::Cmp { param, .. } => {
            if let Some(p) = source.get(*param) {
                out.push(p.clone());
            }
        }
        SqlPredicate::InList { params, .. } => {
            for idx in params {
                if let Some(p) = source.get(*idx) {
                    out.push(p.clone());
                }
            }
        }
        SqlPredicate::Between { low, high, .. } => {
            if let Some(p) = source.get(*low) {
                out.push(p.clone());
            }
            if let Some(p) = source.get(*high) {
                out.push(p.clone());
            }
        }
        SqlPredicate::And(a, b) => {
            collect_params(a, source, out);
            collect_params(b, source, out);
        }
    }
}

/// The SQL text of a comparison operator. `~` (regex) is NOT emitted here — the compiler keeps a
/// `~` predicate as a residual (it is not portable across the three dialects), so it never reaches
/// the emitter; this match is total over the operators the compiler does push down.
fn cmp_op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::Le => "<=",
        CmpOp::Ge => ">=",
        // The compiler never pushes `~` down (kept residual), so this arm is unreachable in
        // practice; rendering it as the portable-nowhere `=` would be wrong, so we emit a token
        // that the compiler's residual-keeping makes dead. Returning "=" here would be a
        // correctness bug, so instead we surface a clearly-invalid operator that the compiler's
        // guard prevents from ever being constructed.
        CmpOp::Match => "= /*unreachable: ~ is kept residual*/",
    }
}

// ----------------------------------------------------------------------------------------------
// DML
// ----------------------------------------------------------------------------------------------

/// A lowered DML operation — the owned, vendor-free shape the applier turns into one parameterized
/// statement per row. Built by [`crate::compile::build_effects`]-style lowering.
#[derive(Debug, Clone, PartialEq)]
pub enum DmlOp {
    /// `INSERT INTO <table> (cols...) VALUES (?, ...)`.
    Insert {
        /// The schema (empty for default).
        schema: String,
        /// The table.
        table: String,
        /// The target columns, in order.
        columns: Vec<String>,
        /// One row's bound values, aligned to `columns`.
        values: Vec<Param>,
    },
    /// `INSERT INTO <table> (...) VALUES (...) ON CONFLICT/ON DUPLICATE KEY ...` — retry-safe.
    Upsert {
        /// The schema.
        schema: String,
        /// The table.
        table: String,
        /// The target columns.
        columns: Vec<String>,
        /// The bound values.
        values: Vec<Param>,
        /// The conflict-key columns (the PK/unique target).
        conflict_keys: Vec<String>,
    },
    /// `UPDATE <table> SET col = ? ... WHERE <pred>`.
    Update {
        /// The schema.
        schema: String,
        /// The table.
        table: String,
        /// The assignments `(col, value)` in order.
        assignments: Vec<(String, Param)>,
        /// The compiled WHERE, if any (a missing WHERE updates the whole table — irreversible).
        where_: Option<SqlPredicate>,
        /// The bound WHERE params, indexed by the predicate leaves.
        where_params: Vec<Param>,
    },
    /// `DELETE FROM <table> WHERE <pred>`.
    Delete {
        /// The schema.
        schema: String,
        /// The table.
        table: String,
        /// The compiled WHERE, if any (a missing WHERE deletes every row — irreversible).
        where_: Option<SqlPredicate>,
        /// The bound WHERE params.
        where_params: Vec<Param>,
    },
}

/// Render a [`DmlOp`] into `(sql, params)` for `dialect`. Identifiers quoted; every value bound;
/// `params` in placeholder order. No value text in the SQL.
#[must_use]
pub fn render_dml(dialect: Dialect, op: &DmlOp) -> (String, Vec<Param>) {
    match op {
        DmlOp::Insert {
            schema,
            table,
            columns,
            values,
        } => render_insert(dialect, schema, table, columns, values, None),
        DmlOp::Upsert {
            schema,
            table,
            columns,
            values,
            conflict_keys,
        } => render_insert(dialect, schema, table, columns, values, Some(conflict_keys)),
        DmlOp::Update {
            schema,
            table,
            assignments,
            where_,
            where_params,
        } => render_update(
            dialect,
            schema,
            table,
            assignments,
            where_.as_ref(),
            where_params,
        ),
        DmlOp::Delete {
            schema,
            table,
            where_,
            where_params,
        } => render_delete(dialect, schema, table, where_.as_ref(), where_params),
    }
}

/// Render an INSERT (or UPSERT when `conflict_keys` is `Some`). The upsert clause is the
/// dialect's divergent form: `ON CONFLICT (...) DO UPDATE SET ...` for pg/sqlite, `ON DUPLICATE
/// KEY UPDATE ...` for mysql.
fn render_insert(
    dialect: Dialect,
    schema: &str,
    table: &str,
    columns: &[String],
    values: &[Param],
    conflict_keys: Option<&[String]>,
) -> (String, Vec<Param>) {
    let table_sql = dialect.quote_qualified(schema, table);
    let col_list = columns
        .iter()
        .map(|c| dialect.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut next_ph = 1usize;
    let placeholders = columns
        .iter()
        .map(|_| {
            let ph = dialect.placeholder(next_ph);
            next_ph += 1;
            ph
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("INSERT INTO {table_sql} ({col_list}) VALUES ({placeholders})");

    if let Some(keys) = conflict_keys {
        // The non-key columns are the ones updated on conflict.
        let updates: Vec<String> = columns
            .iter()
            .filter(|c| !keys.iter().any(|k| k == *c))
            .map(|c| {
                let q = dialect.quote_ident(c);
                match dialect {
                    // pg/sqlite reference the proposed row via `excluded`.
                    Dialect::Postgres | Dialect::Sqlite => format!("{q} = excluded.{q}"),
                    // mysql references it via VALUES(col).
                    Dialect::Mysql => format!("{q} = VALUES({q})"),
                }
            })
            .collect();
        match dialect {
            Dialect::Postgres | Dialect::Sqlite => {
                let key_list = keys
                    .iter()
                    .map(|k| dialect.quote_ident(k))
                    .collect::<Vec<_>>()
                    .join(", ");
                if updates.is_empty() {
                    sql.push_str(&format!(" ON CONFLICT ({key_list}) DO NOTHING"));
                } else {
                    sql.push_str(&format!(
                        " ON CONFLICT ({key_list}) DO UPDATE SET {}",
                        updates.join(", ")
                    ));
                }
            }
            Dialect::Mysql => {
                if updates.is_empty() {
                    // No non-key column to update; touch a key column to a no-op so the statement
                    // is still a valid idempotent upsert.
                    if let Some(first_key) = keys.first() {
                        let q = dialect.quote_ident(first_key);
                        sql.push_str(&format!(" ON DUPLICATE KEY UPDATE {q} = {q}"));
                    }
                } else {
                    sql.push_str(&format!(" ON DUPLICATE KEY UPDATE {}", updates.join(", ")));
                }
            }
        }
    }

    (sql, values.to_vec())
}

/// Render an UPDATE. The SET assignments bind first, then the WHERE placeholders continue the same
/// counter so the params vector is `[assignment values..., where values...]`.
fn render_update(
    dialect: Dialect,
    schema: &str,
    table: &str,
    assignments: &[(String, Param)],
    where_: Option<&SqlPredicate>,
    where_params: &[Param],
) -> (String, Vec<Param>) {
    let table_sql = dialect.quote_qualified(schema, table);
    let mut next_ph = 1usize;
    let set_clause = assignments
        .iter()
        .map(|(col, _)| {
            let ph = dialect.placeholder(next_ph);
            next_ph += 1;
            format!("{} = {ph}", dialect.quote_ident(col))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("UPDATE {table_sql} SET {set_clause}");
    let mut params: Vec<Param> = assignments.iter().map(|(_, v)| v.clone()).collect();
    if let Some(pred) = where_ {
        sql.push_str(" WHERE ");
        sql.push_str(&render_where(dialect, pred, &mut next_ph));
        let mut ordered = Vec::new();
        collect_params(pred, where_params, &mut ordered);
        params.extend(ordered);
    }
    (sql, params)
}

/// Render a DELETE.
fn render_delete(
    dialect: Dialect,
    schema: &str,
    table: &str,
    where_: Option<&SqlPredicate>,
    where_params: &[Param],
) -> (String, Vec<Param>) {
    let table_sql = dialect.quote_qualified(schema, table);
    let mut sql = format!("DELETE FROM {table_sql}");
    let mut next_ph = 1usize;
    let mut params = Vec::new();
    if let Some(pred) = where_ {
        sql.push_str(" WHERE ");
        sql.push_str(&render_where(dialect, pred, &mut next_ph));
        collect_params(pred, where_params, &mut params);
    }
    (sql, params)
}
