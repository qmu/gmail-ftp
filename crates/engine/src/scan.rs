//! [`ScanResults`] — the materialized rows each native [`ScanNode`](cfs_pushdown::ScanNode)
//! returned, fed into the local combine engine. In production these come from the drivers
//! (T10's batcher runs the scans); in tests they are in-memory fakes (no network).
//!
//! Scans are addressed **positionally** in left-to-right order, matching
//! [`PhysicalPlan::scans`](cfs_pushdown::PhysicalPlan::scans), so the engine consumes one
//! [`RowBatch`] per leaf as it walks the plan.

use cfs_types::RowBatch;

/// The per-scan results feeding the combine engine, in left-to-right scan order.
#[derive(Debug, Clone, Default)]
pub struct ScanResults {
    batches: Vec<RowBatch>,
}

impl ScanResults {
    /// Build scan results from an ordered list of batches (one per native scan, in the
    /// same order as [`PhysicalPlan::scans`](cfs_pushdown::PhysicalPlan::scans)).
    #[must_use]
    pub fn new(batches: Vec<RowBatch>) -> Self {
        Self { batches }
    }

    /// The number of scan results.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batches.len()
    }

    /// Whether there are no scan results.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// An iterator that yields each batch once, in order — the engine pulls the next
    /// scan's batch as it reaches each leaf.
    pub(crate) fn into_cursor(self) -> Cursor {
        Cursor {
            batches: self.batches.into_iter(),
        }
    }
}

/// A forward cursor over the scan batches (one per leaf, consumed in plan order).
pub(crate) struct Cursor {
    batches: std::vec::IntoIter<RowBatch>,
}

impl Cursor {
    pub(crate) fn next_batch(&mut self) -> Option<RowBatch> {
        self.batches.next()
    }
}
