//! `conn` — the **connection abstraction** (RFD-0001 §5/§9/§10, ticket step 3). One driver, three
//! dialects: the [`SqlBackend`] trait abstracts the live connection so the driver's compile/emit
//! logic is backend-agnostic, and `postgres`/`mysql`/`sqlite` differ only by their [`Dialect`] and
//! their concrete `SqlBackend` impl. A [`ConnHandle`] pairs a backend with its dialect and cached
//! [`Catalog`]; the [`ConnRegistry`] keys handles by `<conn>`.
//!
//! ## Credentials (RFD §10)
//! A connection's credential (connection string / password) is a [`qfs_secrets::Secret`], fetched
//! by [`CredentialKey`] = `(driver "sql", account <conn>)` from the [`Secrets`] surface and
//! **exposed only at connect time** to the concrete backend. It is **never** logged, never stored
//! in a DTO, never in a [`SqlError`]. The driver scheme is parsed for the [`Dialect`] from the
//! credential's scheme prefix without ever rendering the credential.

use std::collections::HashMap;
use std::sync::Arc;

use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secret, Secrets};
use qfs_types::Row;

use crate::error::SqlError;
use qfs_sql_core::{Catalog, Dialect, DmlOp, Param, SelectPlan};

/// The driver-side credential lookup: fetch the connection secret for `<conn>` and parse its
/// scheme into a [`Dialect`] — **without** rendering the credential anywhere.
///
/// The secret's value is a connection URI (`postgres://...`, `mysql://...`, `sqlite:...`). Only
/// the scheme (the token before `://` or `:`) is read for the dialect; the rest is handed to the
/// backend's `connect` opaquely and never logged.
///
/// # Errors
/// - [`SqlError::Credential`] if the secret is missing/locked/unreadable.
/// - [`SqlError::UnknownScheme`] if the URI scheme is not a recognised dialect.
pub fn resolve_dialect(secrets: &dyn Secrets, conn: &str) -> Result<(Dialect, Secret), SqlError> {
    let account = ConnectionId::new(conn).map_err(|_| SqlError::UnknownConnection {
        conn: conn.to_string(),
    })?;
    let key = CredentialKey::new(DriverId::new("sql"), account);
    let secret = secrets.get(&key).map_err(crate::error::credential_error)?;
    // Read ONLY the scheme prefix from the exposed value; do not retain or log the rest.
    let scheme = secret
        .expose_str()
        .and_then(|uri| uri.split("://").next().or_else(|| uri.split(':').next()))
        .map(str::to_string)
        .ok_or_else(|| SqlError::UnknownScheme {
            scheme: String::new(),
        })?;
    let dialect = Dialect::from_scheme(&scheme).ok_or(SqlError::UnknownScheme { scheme })?;
    Ok((dialect, secret))
}

/// The connection abstraction every backend implements (RFD §9). The driver's compile/emit logic
/// is written once against this trait; `postgres`/`mysql`/`sqlite` are interchangeable impls. The
/// backend owns the live handle and is the **only** place a vendor row/column type exists — every
/// method returns owned qfs DTOs (`Catalog`, `Row`), so no vendor type crosses this boundary.
///
/// `Send + Sync` so a handle can be shared across the runtime bridge.
pub trait SqlBackend: Send + Sync {
    /// The dialect this backend speaks (drives quoting / placeholders / upsert form).
    fn dialect(&self) -> Dialect;

    /// Introspect the connection's catalog (tables/views, columns, types, PK/unique) into owned
    /// DTOs (RFD §5). No vendor type crosses; the SQL type strings are mapped to [`ColumnType`]
    /// via [`Dialect::map_type`] inside the impl.
    ///
    /// # Errors
    /// [`SqlError::Backend`] on a connection / introspection failure.
    fn introspect(&self) -> Result<Catalog, SqlError>;

    /// Execute a compiled, **parameterized** SELECT and return owned rows (RFD §6). The `(sql,
    /// params)` come from [`qfs_sql_core::render_select`]; the backend binds `params` positionally
    /// — never interpolating a value into `sql`.
    ///
    /// # Errors
    /// [`SqlError::Backend`] on an execution failure.
    fn execute_read(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>, SqlError>;

    /// Apply a batch of lowered DML ops inside **one ACID transaction** (BEGIN → ops → COMMIT;
    /// ROLLBACK on any error so a mid-way failure leaves zero rows changed, RFD §6). Returns the
    /// total affected row count on success.
    ///
    /// # Errors
    /// [`SqlError::Backend`] (with the txn rolled back) on any op failure.
    fn commit_transaction(&self, ops: &[DmlOp]) -> Result<u64, SqlError>;
}

/// A live connection: a backend + its cached catalog. The catalog is introspected once and reused
/// by the introspective `Driver` methods (which cannot do I/O). Cheaply cloneable (the backend is
/// behind an `Arc`).
#[derive(Clone)]
pub struct ConnHandle {
    backend: Arc<dyn SqlBackend>,
    catalog: Catalog,
}

impl ConnHandle {
    /// Build a handle, introspecting the catalog once.
    ///
    /// # Errors
    /// [`SqlError::Backend`] if the initial introspection fails.
    pub fn new(backend: Arc<dyn SqlBackend>) -> Result<Self, SqlError> {
        let catalog = backend.introspect()?;
        Ok(Self { backend, catalog })
    }

    /// Build a handle from an already-introspected catalog (test seam / cache restore).
    #[must_use]
    pub fn with_catalog(backend: Arc<dyn SqlBackend>, catalog: Catalog) -> Self {
        Self { backend, catalog }
    }

    /// The dialect of this connection.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.backend.dialect()
    }

    /// The cached catalog (no I/O).
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The underlying backend (for the read/commit I/O path).
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn SqlBackend> {
        &self.backend
    }

    /// Run a compiled SELECT against this connection.
    ///
    /// # Errors
    /// [`SqlError::Backend`] on execution failure.
    pub fn execute_read(&self, plan: &SelectPlan) -> Result<Vec<Row>, SqlError> {
        let (sql, params) = qfs_sql_core::render_select(self.dialect(), plan);
        self.backend.execute_read(&sql, &params)
    }
}

/// The connection registry, keyed by `<conn>`. Built by the engine from the configured
/// connections; the driver looks a handle up by the path's `<conn>` segment.
#[derive(Clone, Default)]
pub struct ConnRegistry {
    conns: HashMap<String, ConnHandle>,
}

impl ConnRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection handle under `conn`.
    #[must_use]
    pub fn with(mut self, conn: impl Into<String>, handle: ConnHandle) -> Self {
        self.conns.insert(conn.into(), handle);
        self
    }

    /// Look up a connection handle.
    ///
    /// # Errors
    /// [`SqlError::UnknownConnection`] if no connection is registered under `conn`.
    pub fn get(&self, conn: &str) -> Result<&ConnHandle, SqlError> {
        self.conns
            .get(conn)
            .ok_or_else(|| SqlError::UnknownConnection {
                conn: conn.to_string(),
            })
    }
}
