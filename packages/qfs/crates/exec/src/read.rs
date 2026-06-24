//! The **read seam** (`ReadDriver`) — the read counterpart of the runtime's write
//! [`ApplyDriver`](qfs_runtime). t20 carry-over closure.
//!
//! ## Why a new seam (the t20 carry-over)
//! Before t29 there was no SELECT read-path execution. The runtime's `ApplyDriver` is a
//! *write/commit* batching engine whose `EffectOutput` carries only an affected **count**
//! — it structurally cannot return [`RowBatch`] rows, so it is not a read seam. Each E4
//! driver grew its own pure `ReadPlan`/`plan_read` (a self-describing description of what to
//! fetch), but **nothing executed those and fed the rows into the engine**. This trait is
//! that execution seam: given one native [`ScanNode`] (source + the pushed predicate/
//! projection/limit + resolved schema), the driver runs its own I/O and returns an owned
//! [`RowBatch`] — exactly the unit [`qfs_engine::ScanResults`] consumes positionally.
//!
//! ## Async, owned, vendor-free (RFD §9)
//! Scans are I/O, so the seam is `async` (object-safe via `async-trait`, like `ApplyDriver`).
//! The driver translates the owned [`PushedQuery`](qfs_pushdown::PushedQuery) inside its own
//! boundary; **no vendor SDK type crosses this seam** — only the owned [`ScanNode`] in and
//! the owned [`RowBatch`] out. Tests register an in-memory fake (no creds, no network).
//!
//! ## Pushdown honesty + the residual property (t20)
//! A driver MAY honestly under-apply the pushed work (e.g. return more rows than `LIMIT`,
//! or ignore a `WHERE` it cannot run): the executor re-applies the **residual** locally via
//! the [`MiniEvaluator`](qfs_engine::MiniEvaluator). The seam therefore only promises the
//! driver returns a superset of the rows the pushed query selects; correctness is restored
//! by the engine's residual re-filter over the over-returned rows.

use std::collections::HashMap;
use std::sync::Arc;

use qfs_core::{CfsError, DriverId, RowBatch};
use qfs_pushdown::ScanNode;

/// The async read seam a source implements to execute one native [`ScanNode`] and return
/// its rows. The read counterpart of the runtime's write `ApplyDriver`.
///
/// `Send + Sync` so the [`ReadRegistry`] can hold `Arc<dyn ReadDriver>` and the executor can
/// run scans concurrently under tokio.
#[async_trait::async_trait]
pub trait ReadDriver: Send + Sync {
    /// Execute one native scan, returning the owned rows.
    ///
    /// The driver runs the work described by `scan.pushed` against `scan.source`/`scan.schema`
    /// and returns at least the rows that work selects (it may return a superset; the executor
    /// re-applies the residual). Returns a structured [`CfsError`] on an I/O / decode failure.
    ///
    /// # Errors
    /// [`CfsError`] if the scan could not be executed (auth, I/O, decode).
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError>;
}

/// The read-time driver registry: maps a [`DriverId`] to the [`ReadDriver`] that services its
/// scans. The executor resolves each `ScanNode.source` through this. Separate from the
/// introspective `MountRegistry` (pure `describe`/`pushdown`) and the runtime's write
/// `DriverRegistry`: a real E4 driver registers all three facets; tests register only the
/// read facet they exercise.
#[derive(Clone, Default)]
pub struct ReadRegistry {
    drivers: HashMap<DriverId, Arc<dyn ReadDriver>>,
}

impl ReadRegistry {
    /// An empty read registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the read driver for `id` (builder form).
    #[must_use]
    pub fn with(mut self, id: DriverId, driver: Arc<dyn ReadDriver>) -> Self {
        self.drivers.insert(id, driver);
        self
    }

    /// Register the read driver for `id` (mutating form).
    pub fn register(&mut self, id: DriverId, driver: Arc<dyn ReadDriver>) {
        self.drivers.insert(id, driver);
    }

    /// Resolve the read driver for `id`, if registered.
    #[must_use]
    pub fn get(&self, id: &DriverId) -> Option<Arc<dyn ReadDriver>> {
        self.drivers.get(id).cloned()
    }
}

impl std::fmt::Debug for ReadRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadRegistry")
            .field("drivers", &self.drivers.keys().collect::<Vec<_>>())
            .finish()
    }
}
