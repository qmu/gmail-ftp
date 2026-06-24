//! `qfs-driver-cf` — the **Cloudflare driver** (RFD-0001 §5, E4 t23): one driver crate mounted at
//! `/cf` exposing three Cloudflare primitives through the uniform qfs DSL, each on its correct
//! archetype:
//!
//! - **D1** (`/cf/d1/<db>/<table>`) — [`Archetype::RelationalTable`]. D1 is SQLite-over-HTTP, so
//!   this **reuses the t17 `qfs-driver-sql` sqlite machinery**: the [`Dialect::Sqlite`] emitter
//!   ([`render_select`]/[`render_dml`]), the pure query compiler ([`compile`] + [`QuerySpec`]),
//!   and the owned [`Catalog`] DTOs. The only new piece is an HTTP [`CfBackend`] that ships the
//!   already-rendered `(sql, params)` to the Cloudflare D1 REST API with `params` as a
//!   **structured bound array** — NEVER interpolated into the SQL (the injection-safety
//!   obligation an HTTP backend carries, flagged by the t17 Architect). The D1 `/batch` endpoint
//!   maps one [`CfApplier`] commit to one atomic transaction (D1 has no interactive BEGIN/COMMIT).
//! - **KV** (`/cf/kv/<ns>/<key>`) — [`Archetype::BlobNamespace`]. `ls/cp/mv/rm` + a degenerate
//!   `(key, value)` table for `SELECT`/`UPSERT`; TTL + metadata per entry.
//! - **Queues** (`/cf/queue/<name>`) — [`Archetype::AppendLog`]. `INSERT` appends a message (with
//!   an idempotency key — at-least-once-safe), `SELECT … LIMIT n` tails (consumer pull).
//!
//! ## Surface
//! - [`CfDriver`] — the introspective `Driver`: `mount()` = `/cf`, per-node archetype + typed
//!   schema, per-node capabilities (a D1 table → full CRUD; a KV namespace →
//!   `{ls,select,upsert,remove,cp,mv,rm}`; a queue → `{insert,select}` only, so `UPDATE`/`JOIN`
//!   over a queue/KV is rejected at the parse gate), and a `Partial` pushdown for D1 (the whole
//!   sqlite vocabulary, reused from t17).
//! - [`CfApplier`] — the synchronous apply leg `applier()` returns and the
//!   [`qfs_runtime::SharedApplier`] the bridge drives under `COMMIT`.
//! - [`cf_apply_driver`] — wraps the applier in a [`qfs_runtime::PlanApplierBridge`] ready to
//!   `register` into a `DriverRegistry` under the driver id `cf`, so a plan over `/cf` executes
//!   end-to-end through the t10 interpreter.
//!
//! ## Purity invariant (RFD §3)
//! Every write constructs a `Plan` node and performs no I/O during planning; only [`CfApplier`]
//! under `COMMIT` touches the Cloudflare API. The introspective methods are pure data.
//!
//! ## No vendor leak (RFD §9)
//! Cloudflare JSON and `worker::*` env bindings are translated into owned DTOs at the
//! [`CfBackend`] boundary; the `Driver` surface and the `Plan` carry zero Cloudflare types.
//! `reqwest` stays in `qfs-driver-http`; this crate rides a LOCAL [`HttpExchange`](backend::HttpExchange)
//! seam (the qfs-google-auth precedent) through the [`HttpApiBackend`] instead. The token is a [`qfs_secrets::Secret`] behind the backend.
//!
//! ## Named parks (deferred per the ticket)
//! - **wasm `WorkersBindingBackend`** — the native `worker` env-binding backend is parked behind
//!   the same [`CfBackend`] seam (no live wasm CI lane yet); the DTOs are wasm-clean so it drops
//!   in later producing identical DTOs (the conformance goal).
//! - **D1 SELECT residual** — the t17 compiler's truthful residual is carried back so the engine
//!   re-filters un-pushable constructs; live D1 integration is gated to a future CI lane (tests
//!   run against a mocked Cloudflare API).
//! - **`@version` / `AS OF`** — declared [`VersionSupport::None`] (D1/KV/Queues expose latest
//!   state only here).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod backend;
mod effect;
mod error;
pub mod path;
pub mod registry;
mod schema;

use std::sync::Arc;

use qfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;
use qfs_sql_core::{compile, Dialect, QuerySpec};
use qfs_types::{Predicate, Row};

pub use applier::CfApplier;
pub use backend::{
    param_to_json, CfBackend, HttpApiBackend, HttpExchange, KvEntry, MockCfBackend, MockExchange,
    MsgId, QueueMsg, RecordedCall, TransportError,
};
pub use effect::{
    CfEffect, KV_KEY_COL, KV_METADATA_COL, KV_TTL_COL, KV_VALUE_COL, QUEUE_BODY_COL,
    QUEUE_IDEMPOTENCY_COL,
};
pub use error::CfError;
pub use path::{CfNode, D1_SEGMENT, KV_SEGMENT, MOUNT, QUEUE_SEGMENT};
pub use registry::{CfRegistry, D1Database};
pub use schema::{kv_table_schema, queue_tail_schema};

// Re-export the reused t17 sqlite emitter entry points (now single-sourced in the pure-leaf
// `qfs-sql-core`) so a caller can render/inspect a D1 statement directly — D1 IS the sqlite
// dialect (t23).
pub use qfs_sql_core::{render_dml, render_select, Catalog, DmlOp, Param, SelectPlan};

/// The Cloudflare API token scopes this driver needs (RFD §10 least-privilege). Documented
/// labels only — never a token. The REST path needs D1 read/write, KV read/write, and Queues
/// send/consume; the server `POLICY` reasons over these.
pub const CF_D1_SCOPE: &str = "d1:read d1:write";
/// The KV least-privilege scope label.
pub const CF_KV_SCOPE: &str = "kv:read kv:write";
/// The Queues least-privilege scope label.
pub const CF_QUEUES_SCOPE: &str = "queues:send queues:consume";

/// The Cloudflare driver (RFD §5). Owns the synchronous [`CfApplier`] the contract returns from
/// `applier()`, plus the declared pushdown profile. Construct with [`CfDriver::new`], injecting
/// the [`CfRegistry`] (each handle carries a [`CfBackend`] whose API token was injected at
/// construction — never on the contract surface).
pub struct CfDriver {
    applier: CfApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl CfDriver {
    /// Build a Cloudflare driver over `registry`.
    #[must_use]
    pub fn new(registry: CfRegistry) -> Self {
        Self {
            applier: CfApplier::new(registry),
            // D1 is a full sqlite backend: the relational subtree over one database collapses
            // into one native SELECT (WHERE / projection / ORDER BY / LIMIT / aggregate /
            // group_by / distinct / single-source JOIN), reusing the t17 sqlite path. KV and
            // Queues push only a bounded scan; the planner queries D1 by intent and re-filters a
            // truthful residual locally for the non-D1 services (see `pushdown_for`). The
            // declared profile is the D1 (richest) one; per-service narrowing is the planner's
            // residual concern, not a profile flag here.
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: true,
                limit: true,
                order: true,
                join: true,
                aggregate: true,
                distinct: true,
                group_by: true,
            },
            procs: Vec::new(),
        }
    }

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn cf_applier(&self) -> &CfApplier {
        &self.applier
    }

    /// Borrow the Cloudflare resource registry (the read path resolves a handle, then reads).
    #[must_use]
    pub fn registry(&self) -> &CfRegistry {
        self.applier.registry()
    }

    /// Compile + execute a relational query against a `/cf/d1/<db>/<table>` table — reusing the
    /// t17 sqlite compiler and emitter. Resolves the D1 catalog, compiles to **injection-safe**
    /// parameterized SQL with a truthful residual, runs it via the backend's `d1_query` (params
    /// bound as a structured array, never interpolated), and returns the rows + the residual the
    /// engine still filters. The only place D1 SELECT I/O happens.
    ///
    /// # Errors
    /// [`CfError`] on an unknown path/column, an unregistered D1 database, or a backend failure.
    pub fn execute_d1_query(
        &self,
        path: &Path,
        spec: &QuerySpec,
    ) -> Result<(Vec<Row>, Option<Predicate>), CfError> {
        let CfNode::D1Table { db, table } = CfNode::parse(path)? else {
            return Err(CfError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "not a concrete /cf/d1/<db>/<table> address",
            });
        };
        let handle = self.registry().d1(&db)?;
        let table_cat = handle.table(&table, path.as_str())?;
        // Reuse the t17 compiler: qfs query -> SelectPlan + truthful residual. The compiler is
        // pure (no I/O, no credential).
        let result = compile("", table_cat, spec)?;
        let backend = handle.backend().clone();
        // The t17 sqlite emitter renders the SelectPlan to (sql, params); params are a structured
        // bound array shipped to the D1 REST API — never interpolated.
        let (sql, params) = render_select(Dialect::Sqlite, &result.plan);
        let rows = backend.d1_query(&db, &sql, &params)?;
        Ok((rows, result.residual))
    }

    /// List keys in a KV namespace (the `ls` / key listing).
    ///
    /// # Errors
    /// [`CfError`] on an unregistered namespace or a backend failure.
    pub fn kv_list_keys(
        &self,
        ns: &str,
        prefix: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<String>, CfError> {
        self.registry().kv(ns)?.clone().kv_list(ns, prefix, limit)
    }

    /// Read a single KV entry by key (the key-value-table `SELECT … WHERE key = ?` point read).
    ///
    /// # Errors
    /// [`CfError`] on an unregistered namespace or a backend failure.
    pub fn kv_get(&self, ns: &str, key: &str) -> Result<Option<KvEntry>, CfError> {
        self.registry().kv(ns)?.clone().kv_get(ns, key)
    }

    /// Tail up to `max` recent messages from a queue (the bounded `SELECT … LIMIT n`).
    ///
    /// # Errors
    /// [`CfError`] on an unregistered queue or a backend failure.
    pub fn queue_tail(&self, queue: &str, max: u32) -> Result<Vec<QueueMsg>, CfError> {
        self.registry().queue(queue)?.clone().queue_pull(queue, max)
    }

    /// The per-node capability set (RFD §5):
    /// - a D1 **table** → full CRUD `{select,insert,upsert,update,remove}`.
    /// - a KV **namespace** → `{ls,cp,mv,rm,select,upsert,remove}` (blob verbs + key/value table).
    /// - a KV **key** → `{select,upsert,remove}` (a single entry).
    /// - a **queue** → `{insert,select}` only (append + bounded tail; `UPDATE`/`REMOVE` denied).
    /// - anything else (root / bare service / unregistered) → the empty set.
    fn caps_for(&self, path: &Path) -> Capabilities {
        match CfNode::parse(path) {
            Ok(CfNode::D1Table { db, .. }) if self.registry().has_d1(&db) => {
                Capabilities::from_verbs(&[
                    Verb::Select,
                    Verb::Insert,
                    Verb::Upsert,
                    Verb::Update,
                    Verb::Remove,
                ])
            }
            Ok(CfNode::KvNamespace { ns }) if self.registry().has_kv(&ns) => {
                Capabilities::from_verbs(&[
                    Verb::Ls,
                    Verb::Cp,
                    Verb::Mv,
                    Verb::Rm,
                    Verb::Select,
                    Verb::Upsert,
                    Verb::Remove,
                ])
            }
            Ok(CfNode::KvKey { ns, .. }) if self.registry().has_kv(&ns) => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Upsert, Verb::Remove])
            }
            Ok(CfNode::Queue { name }) if self.registry().has_queue(&name) => {
                // Append/log: INSERT (append) + SELECT (bounded tail) ONLY. UPDATE/REMOVE/JOIN are
                // rejected at the parse-time gate with a structured error.
                Capabilities::from_verbs(&[Verb::Insert, Verb::Select])
            }
            _ => Capabilities::none(),
        }
    }
}

impl Driver for CfDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        let node = CfNode::parse(path).map_err(|e| qfs_driver::CfsError::InvalidPath {
            path: path.as_str().to_string(),
            reason: match e {
                CfError::InvalidPath { .. } => "not a valid /cf address",
                _ => "not a describable /cf node",
            },
        })?;
        match node {
            CfNode::D1Table { db, table } => {
                // D1 relational schema is the catalog-derived typed Schema (reused t17 catalog).
                let handle =
                    self.registry()
                        .d1(&db)
                        .map_err(|_| qfs_driver::CfsError::InvalidPath {
                            path: path.as_str().to_string(),
                            reason: "no such registered D1 database",
                        })?;
                let table_cat = handle.table(&table, path.as_str()).map_err(|_| {
                    qfs_driver::CfsError::InvalidPath {
                        path: path.as_str().to_string(),
                        reason: "no such D1 table in the database catalog",
                    }
                })?;
                Ok(NodeDesc::new(
                    Archetype::RelationalTable,
                    table_cat.describe_schema(),
                ))
            }
            CfNode::KvNamespace { .. } | CfNode::KvKey { .. } => Ok(NodeDesc::new(
                Archetype::BlobNamespace,
                schema::kv_table_schema(),
            )),
            CfNode::Queue { .. } => Ok(NodeDesc::new(
                Archetype::AppendLog,
                schema::queue_tail_schema(),
            )),
            CfNode::D1Db { .. } | CfNode::Root => Err(qfs_driver::CfsError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "a /cf service root is not a describable node",
            }),
        }
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
        // D1/KV/Queues expose latest state only here; @version / AS OF is deferred (named park).
        VersionSupport::None
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`CfDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding the
/// async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id `cf`. A
/// plan routed to `/cf` then executes end-to-end through the t10 interpreter, which dispatches
/// each effect to this bridge (a D1 write = one atomic `/batch`).
#[must_use]
pub fn cf_apply_driver(driver: &CfDriver) -> PlanApplierBridge<CfApplier> {
    PlanApplierBridge::new(Arc::new(driver.cf_applier().clone()))
}

#[cfg(test)]
mod tests;
