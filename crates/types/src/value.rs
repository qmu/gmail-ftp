//! Runtime [`Value`] / [`Row`] / [`RowBatch`] — the data that flows through a
//! pipeline, mirroring [`ColumnType`](crate::ColumnType). `Null` is **explicit and
//! orthogonal to type** (RFD §4): a column is `nullable` in the schema, and a
//! `Value::Null` may appear wherever the column allows it.
//!
//! These are the DTOs codecs target (`DECODE`/`ENCODE` bridge `bytes ↔ rows`); the
//! canonical home so `cfs-codec` re-exports them rather than redefining placeholders.

use serde::{Deserialize, Serialize};

use crate::schema::{ColumnType, Schema};

/// A single runtime value (RFD §4). Mirrors [`ColumnType`]; `Null` is orthogonal to
/// type. `Json` carries a parsed JSON tree for deeply-irregular columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Value {
    /// SQL-style absence of a value (orthogonal to the column type).
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit signed integer (also carries `Timestamp`/`Date` at runtime).
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// Owned UTF-8 text (also carries `Decimal`/`Uuid` lexical forms at runtime).
    Text(String),
    /// Opaque owned bytes.
    Bytes(Vec<u8>),
    /// A timestamp as an epoch-based integer.
    Timestamp(i64),
    /// A nested record value (mirrors [`ColumnType::Struct`]).
    Struct(Row),
    /// A homogeneous collection value (mirrors [`ColumnType::Array`]).
    Array(Vec<Value>),
    /// A deeply-irregular JSON value (mirrors [`ColumnType::Json`]).
    Json(serde_json::Value),
}

impl Value {
    /// The [`ColumnType`] this value inhabits. `Null` reports [`ColumnType::Unknown`]
    /// because a bare null carries no type (its type comes from the column, RFD §4).
    /// Nested `Struct`/`Array` recover their element type structurally; an empty
    /// array reports `Array(Unknown)` since it has no element to inspect.
    #[must_use]
    pub fn type_of(&self) -> ColumnType {
        match self {
            Value::Null => ColumnType::Unknown,
            Value::Bool(_) => ColumnType::Bool,
            Value::Int(_) => ColumnType::Int,
            Value::Float(_) => ColumnType::Float,
            Value::Text(_) => ColumnType::Text,
            Value::Bytes(_) => ColumnType::Bytes,
            Value::Timestamp(_) => ColumnType::Timestamp,
            Value::Struct(row) => ColumnType::Struct(row.schema_of()),
            Value::Array(items) => {
                let elem = items.first().map_or(ColumnType::Unknown, Value::type_of);
                ColumnType::Array(Box::new(elem))
            }
            Value::Json(_) => ColumnType::Json,
        }
    }

    /// Whether this value conforms to `ty` under nullability `nullable` (RFD §4).
    /// `Null` conforms iff `nullable`. Used by the row-conformance debug check in
    /// tests, not on the hot path.
    #[must_use]
    pub fn conforms_to(&self, ty: &ColumnType, nullable: bool) -> bool {
        match (self, ty) {
            (Value::Null, _) => nullable,
            // Unknown/Json accept any non-null value (late-bound columns, RFD §4).
            (_, ColumnType::Unknown | ColumnType::Json) => true,
            (Value::Bool(_), ColumnType::Bool) => true,
            (Value::Int(_), ColumnType::Int | ColumnType::Timestamp | ColumnType::Date) => true,
            (Value::Float(_), ColumnType::Float) => true,
            (Value::Text(_), ColumnType::Text | ColumnType::Decimal | ColumnType::Uuid) => true,
            (Value::Bytes(_), ColumnType::Bytes) => true,
            (Value::Timestamp(_), ColumnType::Timestamp | ColumnType::Int) => true,
            (Value::Struct(row), ColumnType::Struct(schema)) => row.conforms_to(schema),
            (Value::Array(items), ColumnType::Array(elem)) => {
                items.iter().all(|v| v.conforms_to(elem, true))
            }
            _ => false,
        }
    }
}

/// A single row: positional values aligned to a [`Schema`]'s columns (RFD §4). Owned
/// data only — the DTO that crosses the codec boundary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The column values, in column order.
    pub values: Vec<Value>,
}

impl Row {
    /// Construct a row from its values.
    #[must_use]
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    /// Whether this row conforms to `schema`: same arity, and each value conforms to
    /// its column's type/nullability (RFD §4). A debug/test aid, not the hot path.
    #[must_use]
    pub fn conforms_to(&self, schema: &Schema) -> bool {
        self.values.len() == schema.columns.len()
            && self
                .values
                .iter()
                .zip(&schema.columns)
                .all(|(v, c)| v.conforms_to(&c.ty, c.nullable))
    }

    /// The best-effort structural schema of this row (each value's `type_of`, made
    /// nullable for `Null` values). Positional column names (`0`, `1`, …) are used
    /// since a bare row carries no names. Used by [`Value::type_of`] for nested
    /// structs.
    #[must_use]
    fn schema_of(&self) -> Schema {
        Schema::new(
            self.values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    crate::schema::Column::new(i.to_string(), v.type_of(), matches!(v, Value::Null))
                })
                .collect(),
        )
    }
}

/// A batch of rows with their schema — the relational unit a codec produces/consumes
/// (RFD §4). Owned data only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RowBatch {
    /// The schema the rows conform to.
    pub schema: Schema,
    /// The rows, each positional to `schema.columns`.
    pub rows: Vec<Row>,
}

impl RowBatch {
    /// Construct a batch from a schema and rows.
    #[must_use]
    pub fn new(schema: Schema, rows: Vec<Row>) -> Self {
        Self { schema, rows }
    }

    /// Whether every row conforms to the batch schema (test/debug aid).
    #[must_use]
    pub fn is_conformant(&self) -> bool {
        self.rows.iter().all(|r| r.conforms_to(&self.schema))
    }
}
