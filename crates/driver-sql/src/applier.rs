//! [`SqlApplier`] — the SQL driver's apply leg (RFD-0001 §6, ticket steps 9/11). It lowers a DML
//! effect node ([`EffectKind::Insert`]/`Upsert`/`Update`/`Remove`) into one or more parameterized
//! [`DmlOp`]s and applies them inside **one ACID transaction** on the addressed connection (BEGIN
//! → ops → COMMIT; ROLLBACK on any error so a mid-way failure leaves zero rows changed).
//!
//! ## Single-connection = single transaction (RFD §6)
//! Every effect in one apply targets one `<conn>`; spanning two connections is **not** this
//! ticket's job and is rejected with the structured [`SqlError::CrossSource`]. The applier is
//! stateless (the live handle is behind an `Arc` in the registry), so it implements
//! [`SharedApplier`] (the `&self` apply the runtime bridge requires).
//!
//! ## Capability gating belt-and-suspenders (RFD §5)
//! A write to a **view** is rejected here with [`SqlError::ReadOnlyView`] even though the
//! parse-time capability gate ([`cfs_driver::check_capability`]) rejects it first — so even a
//! hand-built plan that bypassed the gate cannot mutate a view.
//!
//! ## Secret safety
//! No connection string, password, or bound parameter VALUE is ever logged or placed in a
//! [`SqlError`]; the structured logs render the verb + table + affected count only.

use cfs_plan::{AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use cfs_runtime::{EffectError, EffectOutput, SharedApplier};
use cfs_types::Value;

use crate::catalog::TableCatalog;
use crate::conn::ConnRegistry;
use crate::emit::{DmlOp, Param, SqlPredicate};
use crate::error::SqlError;
use crate::path::SqlPath;

/// The synchronous SQL apply leg. Holds the connection registry (handles are behind `Arc`s, so
/// the leg is cheap to clone and `&self`-apply). Stateless across calls.
#[derive(Clone)]
pub struct SqlApplier {
    registry: ConnRegistry,
}

impl SqlApplier {
    /// Build an applier over a connection registry.
    #[must_use]
    pub fn new(registry: ConnRegistry) -> Self {
        Self { registry }
    }

    /// Apply one effect node: resolve the connection, gate the verb against the catalog, lower to
    /// a [`DmlOp`], and commit it in one transaction. Returns the affected row count.
    fn apply_node(&self, node: &EffectNode) -> Result<u64, SqlError> {
        let path = SqlPath::parse_str(node.target.path.as_str())?;
        let SqlPath::Table {
            conn,
            schema,
            table,
        } = &path
        else {
            return Err(SqlError::InvalidPath {
                path: node.target.path.as_str().to_string(),
                reason: "a write effect must target a concrete /sql table",
            });
        };

        let handle = self.registry.get(conn)?;
        let catalog = handle.catalog();
        let table_cat = catalog.table(table).ok_or_else(|| SqlError::UnknownTable {
            table: table.clone(),
        })?;

        // Capability gate (belt-and-suspenders): a view is SELECT-only.
        if table_cat.is_view() {
            return Err(SqlError::ReadOnlyView {
                path: node.target.path.as_str().to_string(),
                verb: static_verb_label(&node.kind),
            });
        }

        let op = lower_effect(node, schema, table_cat)?;
        handle.backend().commit_transaction(&[op])
    }
}

/// Lower one effect node into a [`DmlOp`] against the catalogued table. The row payload (the
/// effect's `args` batch) supplies the column names (from the batch schema) and the bound values;
/// the WHERE for UPDATE/DELETE is carried as a `__where__`-keyed key/value match in the args
/// schema (the planner builds this) — here we support the common "match by key columns" shape:
/// the row's key-column values form the WHERE so a filtered UPDATE/DELETE binds those as params.
fn lower_effect(node: &EffectNode, schema: &str, table: &TableCatalog) -> Result<DmlOp, SqlError> {
    let batch = &node.args;
    let columns: Vec<String> = batch
        .schema
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();

    match node.kind {
        EffectKind::Insert => {
            let row = single_row(node)?;
            Ok(DmlOp::Insert {
                schema: schema.to_string(),
                table: table.name.clone(),
                columns,
                values: row.values.iter().map(Param::from_value).collect(),
            })
        }
        EffectKind::Upsert => {
            let row = single_row(node)?;
            let conflict_keys: Vec<String> =
                table.key_columns().iter().map(|c| c.name.clone()).collect();
            if conflict_keys.is_empty() {
                return Err(SqlError::MalformedEffect {
                    reason: "UPSERT requires a primary-key or unique column to be retry-safe; \
                             the table has none"
                        .to_string(),
                });
            }
            Ok(DmlOp::Upsert {
                schema: schema.to_string(),
                table: table.name.clone(),
                columns,
                values: row.values.iter().map(Param::from_value).collect(),
                conflict_keys,
            })
        }
        EffectKind::Update => {
            let row = single_row(node)?;
            // SET every non-key column; WHERE on the key columns (the retry-safe match).
            let (assignments, where_, where_params) = split_update(table, &columns, &row.values)?;
            Ok(DmlOp::Update {
                schema: schema.to_string(),
                table: table.name.clone(),
                assignments,
                where_,
                where_params,
            })
        }
        EffectKind::Remove => {
            let row = single_row(node)?;
            // WHERE on the key columns present in the row (the retry-safe match). A REMOVE with no
            // key columns present targets the whole table — irreversible — and is rejected here to
            // avoid an accidental mass delete (the planner must supply the key filter).
            let (where_, where_params) = build_key_where(table, &columns, &row.values)?;
            if where_.is_none() {
                return Err(SqlError::MalformedEffect {
                    reason: "REMOVE without a key filter would delete every row; supply the \
                             key column(s) in the effect payload"
                        .to_string(),
                });
            }
            Ok(DmlOp::Delete {
                schema: schema.to_string(),
                table: table.name.clone(),
                where_,
                where_params,
            })
        }
        EffectKind::Read | EffectKind::List | EffectKind::Call(_) => {
            Err(SqlError::MalformedEffect {
                reason: format!(
                    "{} is not a SQL DML effect (only INSERT/UPSERT/UPDATE/REMOVE apply here)",
                    static_verb_label(&node.kind)
                ),
            })
        }
        // `EffectKind` is `#[non_exhaustive]`; any future kind is not a SQL DML effect here.
        _ => Err(SqlError::MalformedEffect {
            reason: format!(
                "{} is not a SQL DML effect (only INSERT/UPSERT/UPDATE/REMOVE apply here)",
                static_verb_label(&node.kind)
            ),
        }),
    }
}

/// The stable `&'static str` label for an effect kind — used where a structured error field is
/// `&'static str` (the borrowed [`EffectKind::label`] cannot satisfy that lifetime).
fn static_verb_label(kind: &EffectKind) -> &'static str {
    match kind {
        EffectKind::Read => "READ",
        EffectKind::List => "LIST",
        EffectKind::Insert => "INSERT",
        EffectKind::Upsert => "UPSERT",
        EffectKind::Update => "UPDATE",
        EffectKind::Remove => "REMOVE",
        EffectKind::Call(_) => "CALL",
        _ => "WRITE",
    }
}

/// The single row a DML effect carries (this driver applies one row per effect node; the planner
/// fans a multi-row write into multiple nodes). A missing/multi row is a structured shape error.
fn single_row(node: &EffectNode) -> Result<&cfs_types::Row, SqlError> {
    match node.args.rows.as_slice() {
        [row] => Ok(row),
        [] => Err(SqlError::MalformedEffect {
            reason: "DML effect carries no row payload".to_string(),
        }),
        _ => Err(SqlError::MalformedEffect {
            reason: "DML effect carries more than one row; the planner must split it per row"
                .to_string(),
        }),
    }
}

/// The lowering of an UPDATE row: the `SET col = ?` assignments, the compiled `WHERE` (on the key
/// columns), and the WHERE's bound params — the three pieces [`DmlOp::Update`] needs.
type UpdateLowering = (Vec<(String, Param)>, Option<SqlPredicate>, Vec<Param>);

/// Split an UPDATE row into `SET <non-key> = ?` assignments and a `WHERE <key> = ?` match. The key
/// columns (PK / unique) become the WHERE; the rest become the SET. A row missing every key column
/// yields an empty WHERE (whole-table update) which the caller turns into the irreversible case.
fn split_update(
    table: &TableCatalog,
    columns: &[String],
    values: &[Value],
) -> Result<UpdateLowering, SqlError> {
    let key_names: Vec<String> = table.key_columns().iter().map(|c| c.name.clone()).collect();
    let mut assignments = Vec::new();
    for (col, val) in columns.iter().zip(values.iter()) {
        if !key_names.iter().any(|k| k == col) {
            assignments.push((col.clone(), Param::from_value(val)));
        }
    }
    if assignments.is_empty() {
        return Err(SqlError::MalformedEffect {
            reason: "UPDATE has no non-key column to set".to_string(),
        });
    }
    let (where_, where_params) = build_key_where(table, columns, values)?;
    if where_.is_none() {
        return Err(SqlError::MalformedEffect {
            reason: "UPDATE without a key filter would update every row; supply the key \
                     column(s) in the effect payload"
                .to_string(),
        });
    }
    Ok((assignments, where_, where_params))
}

/// Build a `WHERE key1 = ? AND key2 = ?` predicate from the key-column values present in the row.
/// Returns `(None, [])` when the row carries no key column (the whole-table case the caller
/// rejects). Each bound value is a [`Param`]; the predicate references them by index.
fn build_key_where(
    table: &TableCatalog,
    columns: &[String],
    values: &[Value],
) -> Result<(Option<SqlPredicate>, Vec<Param>), SqlError> {
    let key_names: Vec<String> = table.key_columns().iter().map(|c| c.name.clone()).collect();
    let mut params: Vec<Param> = Vec::new();
    let mut leaves: Vec<SqlPredicate> = Vec::new();
    for (col, val) in columns.iter().zip(values.iter()) {
        if key_names.iter().any(|k| k == col) {
            let idx = params.len();
            params.push(Param::from_value(val));
            leaves.push(SqlPredicate::Cmp {
                col: col.clone(),
                op: cfs_types::CmpOp::Eq,
                param: idx,
            });
        }
    }
    let pred = leaves
        .into_iter()
        .reduce(|acc, leaf| SqlPredicate::And(Box::new(acc), Box::new(leaf)));
    Ok((pred, params))
}

impl SharedApplier for SqlApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self.apply_node(node)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for SqlApplier {
    /// The introspective `cfs_driver::Driver::applier()` seam (t09). Stateless, so it delegates to
    /// the same `&self` core as [`SharedApplier::apply_shared`]; the structured [`SqlError`] is
    /// reduced to the plan crate's owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
