//! The declarative **binding** seam (RFD-0001 §8): the cause-attachment point E7 sibling
//! tickets implement (HTTP serving t31, cron firing t32, webhook/trigger ingestion t33).
//!
//! A [`Binding`] **converges to the registry** — there is no imperative add/remove API.
//! After every committed `/server` mutation the runtime calls [`Binding::reconcile`] with a
//! read snapshot of the new [`ServerState`], and the binding makes its live causes match
//! (e.g. the HTTP binding rebuilds its route table to equal `state.endpoints`). This keeps
//! the registry the single source of truth: a binding never holds the write guard, and it
//! is handed an owned snapshot so it cannot block a concurrent write across an `.await`.

use crate::error::ServerError;
use crate::state::ServerState;

/// What kind of cause a binding attaches (RFD §8). A label only — the runtime treats every
/// binding uniformly (it just calls `reconcile`); the kind is for the audit log + tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingKind {
    /// A no-op binding (the test/default double — attaches no live cause).
    Null,
    /// HTTP endpoint serving (t31).
    Http,
    /// Cron job scheduling (t32).
    Cron,
    /// Inbound webhook / trigger ingestion (t33).
    Ingest,
}

impl BindingKind {
    /// A stable label for the audit log + structured errors.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            BindingKind::Null => "null",
            BindingKind::Http => "http",
            BindingKind::Cron => "cron",
            BindingKind::Ingest => "ingest",
        }
    }
}

/// The declarative cause-attachment seam. `reconcile` converges the binding's live causes
/// to `state`; it is called after **every** committed `/server` mutation (and once at the
/// end of boot). Implementors **must not** hold any lock across an `.await` and **must not**
/// mutate `ServerState` (the registry is the source of truth; bindings only read it).
pub trait Binding: Send + Sync {
    /// The kind of cause this binding attaches (for the audit log).
    fn kind(&self) -> BindingKind;

    /// Converge this binding's live causes to `state`. Called with a read snapshot after
    /// every committed `/server` mutation.
    ///
    /// # Errors
    /// [`ServerError::Reconcile`] if the binding could not converge (e.g. a port bind
    /// failed). The runtime surfaces it without tearing down the registry.
    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError>;
}

/// The no-op binding (RFD §8): attaches no live cause, always reconciles cleanly. The
/// default binding for tests and for `qfs serve` before any E7 cause binding is wired —
/// proving the reconcile seam fires without needing a network/cron backend.
#[derive(Debug, Default)]
pub struct NullBinding;

impl Binding for NullBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Null
    }

    fn reconcile(&mut self, _state: &ServerState) -> Result<(), ServerError> {
        Ok(())
    }
}

/// A counting test double: a [`NullBinding`] that records how many times `reconcile` was
/// called (the acceptance assertion "reconcile invoked exactly once per committed
/// mutation"). Also captures the row-count of the last snapshot it saw, so a test can
/// assert it observed the post-mutation state.
#[derive(Debug, Default)]
pub struct CountingBinding {
    /// How many times `reconcile` has been called.
    pub reconciles: usize,
    /// The `row_count()` of the most recent snapshot reconcile saw (`None` if never called).
    pub last_row_count: Option<usize>,
}

impl Binding for CountingBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Null
    }

    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError> {
        self.reconciles += 1;
        self.last_row_count = Some(state.row_count());
        Ok(())
    }
}
