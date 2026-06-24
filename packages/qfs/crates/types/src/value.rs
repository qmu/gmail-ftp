//! Runtime [`Value`] / [`Row`] / [`RowBatch`] — the data that flows through a
//! pipeline, mirroring [`ColumnType`](crate::ColumnType). `Null` is **explicit and
//! orthogonal to type** (RFD §4): a column is `nullable` in the schema, and a
//! `Value::Null` may appear wherever the column allows it.
//!
//! These are the DTOs codecs target (`DECODE`/`ENCODE` bridge `bytes ↔ rows`); the
//! canonical home so `qfs-codec` re-exports them rather than redefining placeholders.

use serde::{Deserialize, Serialize};

use crate::schema::{ColumnType, Name, Schema};

/// The **named** fields of a [`Value::Struct`] (RFD §4). A nested record value is
/// self-describing: it carries each field's name alongside its value, in insertion
/// order (key order is stable, RFD §6). This is what makes `a.b.c` path access and
/// lossless nested `ENCODE` work over decoded data — the field names are not
/// reconstructed positionally from a bare row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fields {
    /// The `(name, value)` pairs, in insertion order.
    pub entries: Vec<(Name, Value)>,
}

impl Fields {
    /// Construct named fields from `(name, value)` pairs (insertion order preserved).
    #[must_use]
    pub fn new(entries: Vec<(Name, Value)>) -> Self {
        Self { entries }
    }

    /// Look up a field value by name (the first match; names are conventionally unique).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// The field names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<Name> {
        self.entries.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The field values, in order (drops the names — used when flattening a struct into
    /// positional row columns, e.g. `EXPAND`).
    #[must_use]
    pub fn into_values(self) -> Vec<Value> {
        self.entries.into_iter().map(|(_, v)| v).collect()
    }

    /// Iterate the `(name, value)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Value)> {
        self.entries.iter().map(|(n, v)| (n, v))
    }

    /// The number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

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
    /// A nested record value with **named** fields (mirrors [`ColumnType::Struct`]).
    /// Self-describing: the field names live here, so `a.b.c` access and nested
    /// `ENCODE` resolve by real key name rather than positionally.
    Struct(Fields),
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
            Value::Struct(fields) => ColumnType::Struct(fields.schema_of()),
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
            (Value::Struct(fields), ColumnType::Struct(schema)) => fields.conforms_to(schema),
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
}

impl Fields {
    /// The structural schema of these named fields (each value's `type_of`, made
    /// nullable for `Null` values), preserving the **real field names** and order.
    /// Used by [`Value::type_of`] so a decoded nested struct reports `{name: Text}`,
    /// not positional `{"0": Text}`.
    #[must_use]
    fn schema_of(&self) -> Schema {
        Schema::new(
            self.entries
                .iter()
                .map(|(name, v)| {
                    crate::schema::Column::new(name.clone(), v.type_of(), matches!(v, Value::Null))
                })
                .collect(),
        )
    }

    /// Whether these fields conform to `schema`: same arity, and each field's value
    /// conforms to the schema column at its position (RFD §4). Names are not required
    /// to match the schema (a struct may be navigated by either); arity + positional
    /// type conformance is the conformance contract, mirroring [`Row::conforms_to`].
    #[must_use]
    fn conforms_to(&self, schema: &Schema) -> bool {
        self.entries.len() == schema.columns.len()
            && self
                .entries
                .iter()
                .zip(&schema.columns)
                .all(|((_, v), c)| v.conforms_to(&c.ty, c.nullable))
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
