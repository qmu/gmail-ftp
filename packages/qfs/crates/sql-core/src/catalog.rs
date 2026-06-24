//! `catalog` — owned, vendor-free catalog DTOs and the SQL→qfs schema reconciliation (RFD-0001
//! §5/§9). Introspection (querying `information_schema` for pg/mysql, `pragma_table_info` for
//! sqlite) is performed by the [`SqlBackend`](crate::conn::SqlBackend) and handed back as a
//! [`Catalog`] of owned [`ColumnDef`]s — **no vendor row/column type ever crosses this boundary**
//! (the catalog is what `DESCRIBE`, capability gating, and the emitter's column validation all
//! read). `ColumnDef::ty` is the canonical [`ColumnType`] (mapped via [`Dialect::map_type`]), not
//! a backend type string.

use qfs_types::{Column, ColumnType, DriverId, Provenance, Schema};

/// One owned column definition, derived from backend introspection. `ty` is already the canonical
/// [`ColumnType`] (the dialect mapped the SQL type string before this DTO was built), so nothing
/// downstream re-parses a vendor type. Carries the PK/unique flags the planner/emitter use to
/// decide retry-safe upsert keys and `irreversible` DML.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// The column name.
    pub name: String,
    /// The canonical qfs column type (already mapped from the backend SQL type).
    pub ty: ColumnType,
    /// Whether the column admits `NULL`.
    pub nullable: bool,
    /// Whether the column is (part of) the primary key.
    pub pk: bool,
    /// Whether the column has a uniqueness constraint (PK implies unique).
    pub unique: bool,
}

impl ColumnDef {
    /// Construct a column definition.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        ty: ColumnType,
        nullable: bool,
        pk: bool,
        unique: bool,
    ) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
            pk,
            unique,
        }
    }
}

/// What kind of relation a catalog entry is — drives capability gating (a view is SELECT-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// A base table — full CRUD.
    Table,
    /// A view — SELECT-only (writes rejected at the capability gate and the applier).
    View,
}

/// The owned catalog of one addressable relation: its kind and its columns. Built by
/// introspection and cached per connection. The single source the introspective `Driver` methods
/// and the emitter read — vendor-free.
#[derive(Debug, Clone, PartialEq)]
pub struct TableCatalog {
    /// The relation name (table or view).
    pub name: String,
    /// Whether it is a table or a view.
    pub kind: RelationKind,
    /// The columns, in declaration order.
    pub columns: Vec<ColumnDef>,
}

impl TableCatalog {
    /// Construct a table/view catalog.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: RelationKind, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            kind,
            columns,
        }
    }

    /// Whether this relation is a view (the SELECT-only gate).
    #[must_use]
    pub const fn is_view(&self) -> bool {
        matches!(self.kind, RelationKind::View)
    }

    /// Look up a column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The retry-safe key columns (the PK, or any unique column) — the upsert conflict target and
    /// the marker for whether a filtered UPDATE/DELETE is reversible. Empty when the relation has
    /// no key (then a filtered write is `irreversible`).
    #[must_use]
    pub fn key_columns(&self) -> Vec<&ColumnDef> {
        let pk: Vec<&ColumnDef> = self.columns.iter().filter(|c| c.pk).collect();
        if pk.is_empty() {
            self.columns.iter().filter(|c| c.unique).collect()
        } else {
            pk
        }
    }

    /// Project this catalog into the canonical typed [`Schema`] (RFD §5) — what `DESCRIBE`
    /// returns. Each column carries provenance back to the `sql` driver and its source column
    /// name, so a federated `JOIN` can disambiguate (RFD §5/§10). No secrets.
    #[must_use]
    pub fn describe_schema(&self) -> Schema {
        Schema::new(
            self.columns
                .iter()
                .map(|c| {
                    Column::new(c.name.clone(), c.ty.clone(), c.nullable).with_provenance(
                        Provenance {
                            driver: Some(DriverId::new("sql")),
                            source_col: Some(c.name.clone()),
                        },
                    )
                })
                .collect(),
        )
    }
}

/// The owned catalog of a whole connection: every introspected relation, keyed by name. Cached
/// per `<conn>`; the introspective `Driver` methods read it without I/O.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    /// The relations, in introspection order.
    pub tables: Vec<TableCatalog>,
}

impl Catalog {
    /// Construct a connection catalog from its relations.
    #[must_use]
    pub fn new(tables: Vec<TableCatalog>) -> Self {
        Self { tables }
    }

    /// Look up a relation by name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableCatalog> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The relation names (for the structured `unknown table` context).
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.tables.iter().map(|t| t.name.clone()).collect()
    }
}
