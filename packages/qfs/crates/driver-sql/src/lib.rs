//! `qfs-driver-sql` — the **SQL databases driver** (RFD-0001 §5, E4 t17): the relational/table
//! archetype over real SQL databases (postgres / mysql / sqlite) behind ONE [`Driver`], mounted at
//! `/sql/<conn>/<schema>/<table>`. It is the canonical **pushdown** driver — a single-source
//! relational pipeline collapses into one native, **parameterized** SQL statement, and a
//! single-connection `COMMIT` is one real ACID transaction (RFD §6). It is the base for t23
//! (Cloudflare D1, which shares the sqlite dialect emitter).
//!
//! ## Surface
//! - [`SqlDriver`] — the introspective `Driver`: `mount()` = `/sql`, per-node
//!   [`Archetype::RelationalTable`] + the catalog-derived typed [`Schema`], per-node capabilities
//!   (a **table** → full CRUD `{SELECT,INSERT,UPSERT,UPDATE,REMOVE}`; a **view** → `{SELECT}`
//!   only, so a write is rejected at the parse-time gate), and a `Partial` [`PushdownProfile`]
//!   with **every** flag set (SQL is a full backend — it runs WHERE/SELECT/ORDER/LIMIT/aggregate/
//!   group_by/distinct/join natively; declaring `Partial`-all-true lets the planner query by
//!   intent and lets a future un-renderable construct be turned off one flag at a time).
//! - [`SqlApplier`] — the apply leg the contract returns from `applier()`: lowers DML to
//!   parameterized statements and applies them in one ACID transaction.
//! - [`sql_apply_driver`] — wraps the applier in a [`qfs_runtime::PlanApplierBridge`] ready to
//!   `register` under the driver id `sql`, so a plan over `/sql` executes end-to-end through the
//!   t10 interpreter.
//!
//! ## One dialect decision point + a pluggable backend (RFD §5/§9)
//! [`Dialect`] is the single place the three backends diverge (quoting, `$n`/`?` placeholders,
//! upsert form, type mapping); every match over it is exhaustive. [`SqlBackend`] abstracts the
//! live connection so the compile/emit logic is written once — `postgres`/`mysql`/`sqlite` differ
//! only by their `Dialect` + their `SqlBackend` impl. Vendor row/column types **never** cross the
//! `SqlBackend` boundary (owned DTOs only, RFD §9).
//!
//! ## Query → parameterized SQL with a TRUTHFUL residual (the t20/t21 lesson)
//! [`compile::compile`] lowers a relational query into a [`SelectPlan`] the emitter renders to
//! parameterized SQL. SQL is the lucky exact case: a predicate that compiles to an exactly
//! equivalent SQL `WHERE` **drops** the residual; a construct that cannot be faithfully rendered
//! to portable SQL (`LIKE` glob, `~` regex, `OR`/`NOT`) is **kept** as residual and the engine
//! re-filters — never wrong rows (RFD §6).
//!
//! ## Injection safety (the headline correctness invariant)
//! [`emit`] binds **every** value as a parameter (`$n`/`?`); the SQL string carries only quoted
//! identifiers and placeholders. A value like `'; DROP TABLE t; --` is bound as data, never
//! executed — proven against a live in-process SQLite in the tests.
//!
//! ## Secrets (RFD §10)
//! A connection credential (connection string / password) is a [`qfs_secrets::Secret`] fetched by
//! `(driver "sql", account <conn>)`; only its scheme is read (for the dialect), and it is exposed
//! only to the backend at connect — **never** logged, never in a DTO, never in a [`SqlError`].
//!
//! ## Named parks (deferred per the ticket)
//! - **`@version` / `AS OF` (RFD §4)** — declared `VersionSupport::None` for ordinary tables;
//!   temporal-table generation is deferred (ticket "AS OF declared unsupported … generation
//!   deferred"). A future temporal table flips this per-node.
//! - **Cross-source COMMIT** — a single `COMMIT` spanning two `<conn>`s is rejected with the
//!   structured [`SqlError::CrossSource`]; orchestrated 2-phase commit is the E2 effect-plan
//!   runtime ticket, not this one.
//! - **Live postgres/mysql integration** — the compiled-SQL path is identical across dialects and
//!   covered by per-dialect golden tests; a live pg/mysql container run is gated to a future CI
//!   lane (the ticket's "(gated) ephemeral pg/mysql containers"). The real ACID/injection tests
//!   run against in-process SQLite here.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod conn;
mod error;
mod path;

mod applier;

use std::sync::Arc;

use qfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::SqlApplier;
pub use conn::{resolve_dialect, ConnHandle, ConnRegistry, SqlBackend};
pub use error::{credential_error, sql_error_to_effect_error, SqlError};
pub use path::{SqlPath, MOUNT};

// The pure SQL compile/emit core (dialect, emitter, compiler, catalog DTOs) now lives in the
// pure-leaf `qfs-sql-core` crate (extracted for the t23 Cloudflare D1 reuse — see its crate
// docs). Re-export it so existing `qfs_driver_sql::{Dialect, render_dml, compile, ...}` paths and
// downstream consumers are unchanged.
pub use qfs_sql_core::{
    compile, render_dml, render_select, Catalog, ColumnDef, CompileResult, Dialect, DmlOp,
    OrderTerm, Param, QuerySpec, RelationKind, SelectPlan, SqlPredicate, TableCatalog,
};

/// The SQL databases driver (RFD §5). Owns the [`SqlApplier`] the contract returns from
/// `applier()`, the connection registry it reads catalogs from, and the declared pushdown profile.
/// Construct with [`SqlDriver::new`], injecting the [`ConnRegistry`] (each [`ConnHandle`] carries a
/// live [`SqlBackend`] whose credentials were injected at connect — never on the contract surface).
pub struct SqlDriver {
    registry: ConnRegistry,
    applier: SqlApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl SqlDriver {
    /// Build a SQL driver over `registry`.
    #[must_use]
    pub fn new(registry: ConnRegistry) -> Self {
        Self {
            registry: registry.clone(),
            applier: SqlApplier::new(registry),
            // The read path (`execute_query` → `QuerySpec` → `compile`) pushes the `WHERE`, the
            // `ORDER BY`, the `LIMIT`, and the column `PROJECTION` INTO the database's native SELECT
            // (a non-faithfully-renderable conjunct — LIKE/regex/OR-mixing — is returned as a residual
            // and re-filtered locally, never wrong rows; a native `LIMIT` is emitted only when nothing
            // is residual, and the read facet enforces the pushed `LIMIT` after the local re-filter so
            // it is always honoured exactly). Projection narrows the SELECT column list, but `compile`
            // KEEPS every column the residual reads (`projected ⊇ residual columns`) and the facet
            // narrows back to the requested projection AFTER the residual re-filter — so a pushed
            // projection never strips a column the residual still needs. Aggregate / group_by /
            // distinct / JOIN are NOT yet threaded through the QuerySpec — they stay in the local
            // residual the engine applies (correctness over optimization; each flips on as it grows).
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: true,
                limit: true,
                order: true,
                join: false,
                aggregate: false,
                distinct: false,
                group_by: false,
            },
            // The universal write verbs are the surface (no `CALL` procedures).
            procs: Vec::new(),
        }
    }

    /// Borrow the apply leg (e.g. to build the runtime bridge).
    #[must_use]
    pub fn sql_applier(&self) -> &SqlApplier {
        &self.applier
    }

    /// Borrow the connection registry (the read path resolves a handle, then `execute_read`).
    #[must_use]
    pub fn registry(&self) -> &ConnRegistry {
        &self.registry
    }

    /// Resolve the [`TableCatalog`] for a `/sql/<conn>/.../<table>` path from the cached catalog —
    /// the introspective lookup powering `describe`/`capabilities`/`compile`. Pure: no I/O (the
    /// catalog was introspected once at handle construction).
    ///
    /// # Errors
    /// [`SqlError`] if the path is not a table, the connection is unknown, or the table is absent.
    pub fn resolve_table(&self, path: &Path) -> Result<(String, TableCatalog), SqlError> {
        let parsed = SqlPath::parse(path)?;
        let SqlPath::Table {
            conn,
            schema,
            table,
        } = parsed
        else {
            return Err(SqlError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "not a concrete /sql table address",
            });
        };
        let handle = self.registry.get(&conn)?;
        let table_cat = handle
            .catalog()
            .table(&table)
            .cloned()
            .ok_or(SqlError::UnknownTable { table })?;
        Ok((schema, table_cat))
    }

    /// Compile + execute a relational query against a `/sql` table: resolve the catalog, compile to
    /// parameterized SQL with a truthful residual, run it, and return the rows + the residual the
    /// engine still filters. The only place SELECT I/O happens.
    ///
    /// # Errors
    /// [`SqlError`] on an unknown path/column or a backend execution failure.
    pub fn execute_query(
        &self,
        path: &Path,
        spec: &QuerySpec,
    ) -> Result<
        (
            Vec<qfs_types::Row>,
            Option<qfs_types::Predicate>,
            qfs_types::Schema,
        ),
        SqlError,
    > {
        let parsed = SqlPath::parse(path)?;
        let SqlPath::Table {
            conn,
            schema,
            table,
        } = parsed
        else {
            return Err(SqlError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "not a concrete /sql table address",
            });
        };
        let handle = self.registry.get(&conn)?;
        let table_cat = handle
            .catalog()
            .table(&table)
            .ok_or(SqlError::UnknownTable {
                table: table.clone(),
            })?;
        let result = compile(&schema, table_cat, spec)?;
        let rows = handle.execute_read(&result.plan)?;
        // The output schema reflects what the SELECT actually returned: the (residual-expanded)
        // projection in SELECT order, or the full catalog schema for a `SELECT *` (empty projection).
        // The facet narrows to the requested projection AFTER re-applying the residual.
        let full = table_cat.describe_schema();
        let out_schema = if result.plan.projection.is_empty() {
            full
        } else {
            let cols = result
                .plan
                .projection
                .iter()
                .filter_map(|name| full.column(name).cloned())
                .collect();
            qfs_types::Schema::new(cols)
        };
        Ok((rows, result.residual, out_schema))
    }

    /// The per-node capability set (RFD §5): a **table** admits full CRUD; a **view** admits
    /// `SELECT` only (every write verb absent ⇒ rejected at the parse-time gate); a non-table path
    /// (root / bare connection / unknown) admits nothing.
    fn caps_for(&self, path: &Path) -> Capabilities {
        match self.resolve_table(path) {
            Ok((_, table)) if table.is_view() => Capabilities::from_verbs(&[Verb::Select]),
            Ok((_, _table)) => Capabilities::from_verbs(&[
                Verb::Select,
                Verb::Insert,
                Verb::Upsert,
                Verb::Update,
                Verb::Remove,
            ]),
            Err(_) => Capabilities::none(),
        }
    }
}

impl Driver for SqlDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Every concrete /sql table is the relational archetype; its schema is the catalog-derived
        // typed Schema. The introspective method is pure (the catalog was introspected once at
        // handle construction). A non-table path is not describable.
        let (_schema, table) =
            self.resolve_table(path)
                .map_err(|e| qfs_driver::CfsError::InvalidPath {
                    path: path.as_str().to_string(),
                    reason: match e {
                        SqlError::UnknownTable { .. } => {
                            "no such table/view in the connection catalog"
                        }
                        SqlError::UnknownConnection { .. } => "no such registered /sql connection",
                        _ => "not a concrete /sql table address",
                    },
                })?;
        Ok(NodeDesc::new(
            Archetype::RelationalTable,
            table.describe_schema(),
        ))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        self.caps_for(path)
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn version_support(&self, _path: &Path) -> VersionSupport {
        // AS OF / @version is declared unsupported for ordinary tables (ticket): generation for a
        // temporal table is deferred. A future temporal table flips this per-node.
        VersionSupport::None
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`SqlDriver`]'s applier in the runtime [`PlanApplierBridge`], yielding the async
/// `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id `sql`. A plan
/// routed to `/sql` then executes through the t10 interpreter, which dispatches each DML effect to
/// this bridge (one ACID transaction per apply).
#[must_use]
pub fn sql_apply_driver(driver: &SqlDriver) -> PlanApplierBridge<SqlApplier> {
    PlanApplierBridge::new(Arc::new(driver.sql_applier().clone()))
}

#[cfg(test)]
mod tests;
