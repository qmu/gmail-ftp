//! The owned, vendor-free **event DTO** (t34, RFD §9): the normalized cause an inbound webhook
//! or a polling watcher emits onto the [`EventBus`](crate::bus::EventBus), and the `NEW.*` a
//! trigger handler binds. NO raw vendor request type leaks past ingestion — only this owned DTO
//! crosses the bus. PURE + wasm-portable (no tokio, no I/O): it is the part that maps to a CF
//! Queues message.

use qfs_core::Row;
use serde::{Deserialize, Serialize};

/// A stable event identity (the bus keys redelivery + ack on it). A monotonically-derived
/// owned string; the producer mints it (a webhook request id, a watcher `source#seq`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventId(pub String);

impl EventId {
    /// Construct an event id from owned text.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The raw id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The source path the event came from (the webhook route or the watcher's source path), e.g.
/// `/hooks/inbox` or `/mail/inbox`. An owned address, never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourcePath(pub String);

impl SourcePath {
    /// Construct a source path from owned text.
    #[must_use]
    pub fn new(p: impl Into<String>) -> Self {
        Self(p.into())
    }

    /// The raw path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What kind of change produced the event (RFD §8). `#[non_exhaustive]` so a future kind does
/// not break a match downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EventKind {
    /// An inbound webhook request (the append/log archetype ingress).
    Webhook,
    /// A watcher saw a new row appear at the source.
    RowAppended,
    /// A watcher saw an existing row change (by native id/etag/`@version`).
    RowChanged,
    /// A watcher saw a row removed from the source.
    RowRemoved,
}

impl EventKind {
    /// A stable label for the audit log + structured errors.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            EventKind::Webhook => "webhook",
            EventKind::RowAppended => "row_appended",
            EventKind::RowChanged => "row_changed",
            EventKind::RowRemoved => "row_removed",
        }
    }
}

/// A normalized event (RFD §8/§9). The `new` row exposes the `NEW.*` fields a handler binds; the
/// `dedup_key` is the END-TO-END idempotency identity (stable across redelivery) so delivering the
/// same Event twice yields one net effect under an idempotent handler. Owned data only — no vendor
/// request type, no secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The bus delivery identity (redelivery + ack key).
    pub id: EventId,
    /// Where the event came from (webhook route / watcher source path).
    pub source: SourcePath,
    /// What kind of change it is.
    pub kind: EventKind,
    /// The **stable** idempotency key (source + native id/etag/`@version`). Two deliveries of the
    /// same logical event carry the SAME dedup_key, so the dispatcher's idempotency ledger makes
    /// the second a no-op after the first net effect (at-least-once + idempotent handlers).
    pub dedup_key: String,
    /// The `NEW.*` field NAMES, positionally aligned with `new.values` — so a `NEW.<col>` handler
    /// reference and a `WHERE NEW.<col>` guard resolve to the right value. The producer
    /// (webhook/watcher) sets these; they travel WITH the payload (a CF Queues message carries
    /// both), so the dispatcher needs no out-of-band schema.
    pub columns: Vec<String>,
    /// The `NEW.*` payload — the row a handler's `NEW.<col>` references resolve against, and the
    /// row the trigger `WHERE NEW.*` guard is evaluated over (positionally aligned with `columns`).
    pub new: Row,
    /// When the event was received (epoch seconds — the project standard, matching
    /// `Value::Timestamp`; no chrono).
    pub received_at: i64,
}

impl Event {
    /// Construct an event, deriving the `dedup_key` from the source + a native id (the stable
    /// per-source identity: a webhook request id, a row's native id/etag/`@version`). This keeps
    /// the key derivation in ONE place so a producer cannot hand-roll an inconsistent key.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        source: SourcePath,
        kind: EventKind,
        native_id: &str,
        columns: Vec<String>,
        new: Row,
        received_at: i64,
    ) -> Self {
        let source_str = source.as_str().to_string();
        Self {
            id: EventId::new(id),
            source,
            kind,
            dedup_key: format!("{source_str}#{native_id}"),
            columns,
            new,
            received_at,
        }
    }
}
