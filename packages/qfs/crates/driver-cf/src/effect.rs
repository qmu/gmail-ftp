//! [`CfEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one
//! of these against the [`CfBackend`](crate::backend::CfBackend) under `COMMIT`.
//!
//! ## The `(kind, node)` → concrete-op mapping
//! The closed core [`EffectKind`] is universal; each maps onto a concrete Cloudflare op via the
//! node's parsed [`CfNode`] path + its row args:
//! - D1: `Insert/Upsert/Update/Remove` over `/cf/d1/<db>/<table>` → a t17 [`DmlOp`] applied in
//!   one D1 `/batch` (atomic). The DML is built from the catalogued table exactly as t17 does,
//!   so the **same** injection-safe parameterized SQL is rendered.
//! - KV: `Upsert INTO /cf/kv/<ns>` → a put; `Remove /cf/kv/<ns>/<key>` → a delete.
//! - Queues: `Insert INTO /cf/queue/<name>` → a send (with an idempotency key).
//!
//! No vendor type appears here.

use qfs_plan::{EffectKind, EffectNode};
use qfs_sql_core::{DmlOp, Param, SqlPredicate, TableCatalog};
use qfs_types::Value;

use crate::backend::KvEntry;
use crate::error::CfError;
use crate::path::CfNode;

/// Row column carrying the KV key (the `UPSERT`/`Remove` key when the path addresses a namespace).
pub const KV_KEY_COL: &str = "key";
/// Row column carrying the KV value (the bytes/text to store).
pub const KV_VALUE_COL: &str = "value";
/// Row column carrying the optional KV metadata string.
pub const KV_METADATA_COL: &str = "metadata";
/// Row column carrying the optional KV TTL (seconds).
pub const KV_TTL_COL: &str = "ttl";
/// Row column carrying the queue message body (the `INSERT` payload).
pub const QUEUE_BODY_COL: &str = "body";
/// Row column carrying an explicit idempotency key for a queue `INSERT` (else one is derived).
pub const QUEUE_IDEMPOTENCY_COL: &str = "idempotency_key";

/// One fully-decoded Cloudflare effect — what the apply leg executes. Owned DTOs; no Cloudflare
/// type appears here. The D1 arm carries the reused t17 [`DmlOp`] (rendered to injection-safe
/// SQL by the sqlite emitter at apply time); `QueueSend` is irreversible (an append cannot be
/// un-appended, RFD §10).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CfEffect {
    /// A D1 DML op (`INSERT/UPSERT/UPDATE/REMOVE`) — applied in one atomic `/batch`.
    D1Dml {
        /// The D1 database name.
        db: String,
        /// The lowered DML op (the t17 owned shape; the emitter renders it parameterized).
        op: Box<DmlOp>,
    },
    /// A KV put (`UPSERT INTO /cf/kv/<ns>`) — retry-safe create-or-replace.
    KvPut {
        /// The KV namespace.
        ns: String,
        /// The entry to write.
        entry: KvEntry,
    },
    /// A KV delete (`REMOVE /cf/kv/<ns>/<key>`).
    KvDelete {
        /// The KV namespace.
        ns: String,
        /// The key to delete.
        key: String,
    },
    /// A queue send (`INSERT INTO /cf/queue/<name>`) — irreversible append with an idempotency
    /// key (so an at-least-once retry does not double-append, RFD §6).
    QueueSend {
        /// The queue name.
        queue: String,
        /// The message body.
        body: Vec<u8>,
        /// The idempotency key.
        idempotency_key: String,
    },
}

impl CfEffect {
    /// Decode a runtime [`EffectNode`] into the concrete Cloudflare operation.
    ///
    /// `d1_table_for` resolves a D1 table's catalog (so the DML lowering reuses the t17 shape);
    /// it is `None` for non-D1 nodes.
    ///
    /// # Errors
    /// [`CfError`] if the `(kind, path)` pair is not one the CF driver services, or the row args
    /// carry no usable payload.
    pub fn from_node<'a, F>(node: &EffectNode, d1_table_for: F) -> Result<Self, CfError>
    where
        F: Fn(&str, &str) -> Result<&'a TableCatalog, CfError>,
    {
        let path = CfNode::parse_str(node.target.path.as_str())?;
        match (&node.kind, &path) {
            // D1 writes — reuse the t17 DML lowering over the catalogued table.
            (
                EffectKind::Insert | EffectKind::Upsert | EffectKind::Update | EffectKind::Remove,
                CfNode::D1Table { db, table },
            ) => {
                let table_cat = d1_table_for(db, table)?;
                let op = lower_d1_dml(node, table_cat)?;
                Ok(CfEffect::D1Dml {
                    db: db.clone(),
                    op: Box::new(op),
                })
            }
            // KV upsert — write a (key, value) entry into the namespace.
            (EffectKind::Upsert | EffectKind::Insert, CfNode::KvNamespace { ns }) => {
                let entry = kv_entry_from_row(node)?;
                Ok(CfEffect::KvPut {
                    ns: ns.clone(),
                    entry,
                })
            }
            // KV upsert addressing a concrete key — value rides in the row.
            (EffectKind::Upsert | EffectKind::Insert, CfNode::KvKey { ns, key }) => {
                let mut entry = kv_entry_from_row(node).unwrap_or_default();
                entry.key = key.clone();
                Ok(CfEffect::KvPut {
                    ns: ns.clone(),
                    entry,
                })
            }
            // KV delete — by the key in the path, or the `key` column.
            (EffectKind::Remove, CfNode::KvKey { ns, key }) => Ok(CfEffect::KvDelete {
                ns: ns.clone(),
                key: key.clone(),
            }),
            (EffectKind::Remove, CfNode::KvNamespace { ns }) => {
                let key = text_col(node, KV_KEY_COL).ok_or_else(|| CfError::MalformedEffect {
                    verb: "REMOVE",
                    path: node.target.path.as_str().to_string(),
                    reason: format!("REMOVE /cf/kv/{ns} needs a `{KV_KEY_COL}` to delete"),
                })?;
                Ok(CfEffect::KvDelete {
                    ns: ns.clone(),
                    key,
                })
            }
            // Queue append — INSERT carries the body + (optional) idempotency key.
            (EffectKind::Insert, CfNode::Queue { name }) => {
                let body = body_from_row(node)?;
                let idempotency_key = text_col(node, QUEUE_IDEMPOTENCY_COL)
                    .unwrap_or_else(|| derive_idempotency_key(&body));
                Ok(CfEffect::QueueSend {
                    queue: name.clone(),
                    body,
                    idempotency_key,
                })
            }
            // Everything else is not a CF write the driver services — a capability denial.
            (kind, _) => Err(CfError::CapabilityDenied {
                path: node.target.path.as_str().to_string(),
                verb: static_verb_label(kind),
            }),
        }
    }

    /// Whether this effect is irreversible (RFD §10): a queue send (an append cannot be undone).
    /// D1 destructive writes (`UPDATE`/`REMOVE` without a key filter) are flagged irreversible by
    /// the planner per-node; a KV delete is idempotent (re-deletable). The runtime never retries
    /// an irreversible leg.
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        matches!(self, CfEffect::QueueSend { .. })
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            CfEffect::D1Dml { .. } => "D1",
            CfEffect::KvPut { .. } => "UPSERT",
            CfEffect::KvDelete { .. } => "REMOVE",
            CfEffect::QueueSend { .. } => "INSERT",
        }
    }
}

/// Lower one D1 effect node into a t17 [`DmlOp`] against the catalogued table — mirroring the
/// t17 SQL applier's lowering so the **same** injection-safe parameterized SQL is rendered. The
/// row payload supplies the column names (from the batch schema) and the bound values; the WHERE
/// for UPDATE/REMOVE matches on the table's key columns.
fn lower_d1_dml(node: &EffectNode, table: &TableCatalog) -> Result<DmlOp, CfError> {
    let columns: Vec<String> = node
        .args
        .schema
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let row = single_row(node)?;
    let table_name = table.name.clone();
    let path = node.target.path.as_str().to_string();

    match node.kind {
        EffectKind::Insert => Ok(DmlOp::Insert {
            schema: String::new(),
            table: table_name,
            columns,
            values: row.values.iter().map(Param::from_value).collect(),
        }),
        EffectKind::Upsert => {
            let conflict_keys: Vec<String> =
                table.key_columns().iter().map(|c| c.name.clone()).collect();
            if conflict_keys.is_empty() {
                return Err(CfError::MalformedEffect {
                    verb: "UPSERT",
                    path,
                    reason: "UPSERT requires a primary-key or unique column to be retry-safe; \
                             the D1 table has none"
                        .to_string(),
                });
            }
            Ok(DmlOp::Upsert {
                schema: String::new(),
                table: table_name,
                columns,
                values: row.values.iter().map(Param::from_value).collect(),
                conflict_keys,
            })
        }
        EffectKind::Update => {
            let (assignments, where_, where_params) =
                split_update(table, &columns, &row.values, &path)?;
            Ok(DmlOp::Update {
                schema: String::new(),
                table: table_name,
                assignments,
                where_,
                where_params,
            })
        }
        EffectKind::Remove => {
            let (where_, where_params) = build_key_where(table, &columns, &row.values);
            if where_.is_none() {
                return Err(CfError::MalformedEffect {
                    verb: "REMOVE",
                    path,
                    reason: "REMOVE without a key filter would delete every D1 row; supply the \
                             key column(s) in the effect payload"
                        .to_string(),
                });
            }
            Ok(DmlOp::Delete {
                schema: String::new(),
                table: table_name,
                where_,
                where_params,
            })
        }
        _ => Err(CfError::MalformedEffect {
            verb: static_verb_label(&node.kind),
            path,
            reason: "not a D1 DML effect (only INSERT/UPSERT/UPDATE/REMOVE apply)".to_string(),
        }),
    }
}

/// The SET assignments + the key-WHERE for an UPDATE lowering.
type UpdateLowering = (Vec<(String, Param)>, Option<SqlPredicate>, Vec<Param>);

/// Split an UPDATE row into `SET <non-key> = ?` assignments and a `WHERE <key> = ?` match (the
/// retry-safe shape t17 uses).
fn split_update(
    table: &TableCatalog,
    columns: &[String],
    values: &[Value],
    path: &str,
) -> Result<UpdateLowering, CfError> {
    let key_names: Vec<String> = table.key_columns().iter().map(|c| c.name.clone()).collect();
    let mut assignments = Vec::new();
    for (col, val) in columns.iter().zip(values.iter()) {
        if !key_names.iter().any(|k| k == col) {
            assignments.push((col.clone(), Param::from_value(val)));
        }
    }
    if assignments.is_empty() {
        return Err(CfError::MalformedEffect {
            verb: "UPDATE",
            path: path.to_string(),
            reason: "UPDATE has no non-key column to set".to_string(),
        });
    }
    let (where_, where_params) = build_key_where(table, columns, values);
    if where_.is_none() {
        return Err(CfError::MalformedEffect {
            verb: "UPDATE",
            path: path.to_string(),
            reason: "UPDATE without a key filter would update every D1 row; supply the key \
                     column(s) in the effect payload"
                .to_string(),
        });
    }
    Ok((assignments, where_, where_params))
}

/// Build a `WHERE key1 = ? AND key2 = ?` predicate from the key-column values present in the row
/// (the t17 retry-safe match). Returns `(None, [])` when the row carries no key column.
fn build_key_where(
    table: &TableCatalog,
    columns: &[String],
    values: &[Value],
) -> (Option<SqlPredicate>, Vec<Param>) {
    let key_names: Vec<String> = table.key_columns().iter().map(|c| c.name.clone()).collect();
    let mut params: Vec<Param> = Vec::new();
    let mut leaves: Vec<SqlPredicate> = Vec::new();
    for (col, val) in columns.iter().zip(values.iter()) {
        if key_names.iter().any(|k| k == col) {
            let idx = params.len();
            params.push(Param::from_value(val));
            leaves.push(SqlPredicate::Cmp {
                col: col.clone(),
                op: qfs_types::CmpOp::Eq,
                param: idx,
            });
        }
    }
    let pred = leaves
        .into_iter()
        .reduce(|acc, leaf| SqlPredicate::And(Box::new(acc), Box::new(leaf)));
    (pred, params)
}

/// Build a [`KvEntry`] from the node's first row, reading the well-known KV columns.
fn kv_entry_from_row(node: &EffectNode) -> Result<KvEntry, CfError> {
    let key = text_col(node, KV_KEY_COL).unwrap_or_default();
    let value = bytes_col(node, KV_VALUE_COL).unwrap_or_default();
    if key.is_empty() && value.is_empty() {
        return Err(CfError::MalformedEffect {
            verb: "UPSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("KV write needs a `{KV_KEY_COL}` and/or `{KV_VALUE_COL}`"),
        });
    }
    let mut entry = KvEntry::new(key, value);
    entry.metadata = text_col(node, KV_METADATA_COL);
    entry.expiration_ttl = int_col(node, KV_TTL_COL).and_then(|n| u64::try_from(n).ok());
    Ok(entry)
}

/// Read the queue message body from the node's first row (the `body` column).
fn body_from_row(node: &EffectNode) -> Result<Vec<u8>, CfError> {
    bytes_col(node, QUEUE_BODY_COL)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| CfError::MalformedEffect {
            verb: "INSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("queue INSERT needs a non-empty `{QUEUE_BODY_COL}`"),
        })
}

/// Derive a deterministic idempotency key from the body bytes (a small FNV-1a hash), so a retry
/// of the *same* message carries the *same* key and de-dupes — without a random source (purity).
fn derive_idempotency_key(body: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in body {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("idem-{hash:016x}")
}

/// The single row a DML/write effect carries (this driver applies one row per effect node).
fn single_row(node: &EffectNode) -> Result<&qfs_types::Row, CfError> {
    match node.args.rows.as_slice() {
        [row] => Ok(row),
        [] => Err(CfError::MalformedEffect {
            verb: "EFFECT",
            path: node.target.path.as_str().to_string(),
            reason: "D1 write effect carries no row payload".to_string(),
        }),
        _ => Err(CfError::MalformedEffect {
            verb: "EFFECT",
            path: node.target.path.as_str().to_string(),
            reason: "D1 write effect carries more than one row; the planner must split it per row"
                .to_string(),
        }),
    }
}

/// The stable `&'static str` label for an effect kind.
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

/// Read a non-empty `Text` value from the node's first row by column name.
fn text_col(node: &EffectNode, name: &str) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Read an `Int` value from the node's first row by column name.
fn int_col(node: &EffectNode, name: &str) -> Option<i64> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

/// Read a value column as bytes (a `Bytes` column verbatim, or a `Text` column's UTF-8 bytes).
fn bytes_col(node: &EffectNode, name: &str) -> Option<Vec<u8>> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        Some(Value::Text(t)) => Some(t.clone().into_bytes()),
        _ => None,
    }
}
