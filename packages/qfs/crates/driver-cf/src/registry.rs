//! `registry` — the per-mount Cloudflare resource registry (RFD-0001 §5). The engine builds it
//! from the configured Cloudflare resources; the driver looks a handle up by the path's
//! `<db>`/`<ns>`/`<name>` segment.
//!
//! ## D1 catalog (reused from t17)
//! A [`D1Database`] pairs a [`CfBackend`](crate::backend::CfBackend) with its cached
//! [`Catalog`](qfs_sql_core::Catalog) — the **same** owned catalog DTO the t17 SQL driver
//! introspects, so `DESCRIBE`/`capabilities`/the SQL compiler all read it without I/O (the
//! catalog was introspected once at handle construction). The D1 backend's catalog is supplied at
//! construction (D1's schema is known to the engine config / introspected via `PRAGMA` over the
//! same SQLite dialect).
//!
//! KV namespaces and queues carry no catalog — their schema is the fixed degenerate `(key,
//! value)` / `(id, body, attempts)` shape — so the registry tracks only their declared names.

use std::collections::HashMap;
use std::sync::Arc;

use qfs_sql_core::{Catalog, TableCatalog};

use crate::backend::CfBackend;
use crate::error::CfError;

/// One live Cloudflare backend handle: the shared [`CfBackend`] plus, for D1, the cached
/// [`Catalog`]. Cheaply cloneable (the backend is behind an `Arc`).
#[derive(Clone)]
pub struct D1Database {
    backend: Arc<dyn CfBackend>,
    catalog: Catalog,
}

impl D1Database {
    /// Build a D1 database handle from a backend + an already-introspected [`Catalog`] (the
    /// engine config supplies the schema; D1 introspection rides the same sqlite `PRAGMA` path as
    /// t17 and is the engine's concern, not this driver's I/O).
    #[must_use]
    pub fn new(backend: Arc<dyn CfBackend>, catalog: Catalog) -> Self {
        Self { backend, catalog }
    }

    /// The shared backend (the read/commit I/O path).
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn CfBackend> {
        &self.backend
    }

    /// The cached catalog (no I/O).
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Look up a table's catalog by name.
    ///
    /// # Errors
    /// [`CfError::MalformedEffect`] if the table is absent from the catalog.
    pub fn table(&self, name: &str, path: &str) -> Result<&TableCatalog, CfError> {
        self.catalog
            .table(name)
            .ok_or_else(|| CfError::MalformedEffect {
                verb: "EFFECT",
                path: path.to_string(),
                reason: format!("no such D1 table `{name}` in the database catalog"),
            })
    }
}

/// The Cloudflare resource registry, keyed by the service target name. Built by the engine from
/// the configured resources; the driver resolves a handle by the path's selector segment.
///
/// - D1 databases carry a backend + catalog (`d1`).
/// - KV namespaces carry only a backend (their schema is fixed).
/// - Queues carry only a backend (their schema is fixed).
///
/// A single shared backend commonly serves all three (one Cloudflare account); the registry lets
/// each service map a target name to its handle independently so a least-privilege deployment can
/// scope per-resource.
#[derive(Clone, Default)]
pub struct CfRegistry {
    d1: HashMap<String, D1Database>,
    kv: HashMap<String, Arc<dyn CfBackend>>,
    queues: HashMap<String, Arc<dyn CfBackend>>,
}

impl CfRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a D1 database under `db`.
    #[must_use]
    pub fn with_d1(mut self, db: impl Into<String>, handle: D1Database) -> Self {
        self.d1.insert(db.into(), handle);
        self
    }

    /// Register a KV namespace under `ns` (served by `backend`).
    #[must_use]
    pub fn with_kv(mut self, ns: impl Into<String>, backend: Arc<dyn CfBackend>) -> Self {
        self.kv.insert(ns.into(), backend);
        self
    }

    /// Register a queue under `name` (served by `backend`).
    #[must_use]
    pub fn with_queue(mut self, name: impl Into<String>, backend: Arc<dyn CfBackend>) -> Self {
        self.queues.insert(name.into(), backend);
        self
    }

    /// Look up a D1 database handle.
    ///
    /// # Errors
    /// [`CfError::InvalidPath`] if no D1 database is registered under `db`.
    pub fn d1(&self, db: &str) -> Result<&D1Database, CfError> {
        self.d1.get(db).ok_or(CfError::InvalidPath {
            path: format!("/cf/d1/{db}"),
            reason: "no such registered D1 database",
        })
    }

    /// Whether a D1 database is registered (the introspective capability gate uses this without
    /// borrowing the handle).
    #[must_use]
    pub fn has_d1(&self, db: &str) -> bool {
        self.d1.contains_key(db)
    }

    /// Look up a KV namespace backend.
    ///
    /// # Errors
    /// [`CfError::InvalidPath`] if no KV namespace is registered under `ns`.
    pub fn kv(&self, ns: &str) -> Result<&Arc<dyn CfBackend>, CfError> {
        self.kv.get(ns).ok_or(CfError::InvalidPath {
            path: format!("/cf/kv/{ns}"),
            reason: "no such registered KV namespace",
        })
    }

    /// Whether a KV namespace is registered.
    #[must_use]
    pub fn has_kv(&self, ns: &str) -> bool {
        self.kv.contains_key(ns)
    }

    /// Look up a queue backend.
    ///
    /// # Errors
    /// [`CfError::InvalidPath`] if no queue is registered under `name`.
    pub fn queue(&self, name: &str) -> Result<&Arc<dyn CfBackend>, CfError> {
        self.queues.get(name).ok_or(CfError::InvalidPath {
            path: format!("/cf/queue/{name}"),
            reason: "no such registered queue",
        })
    }

    /// Whether a queue is registered.
    #[must_use]
    pub fn has_queue(&self, name: &str) -> bool {
        self.queues.contains_key(name)
    }
}
