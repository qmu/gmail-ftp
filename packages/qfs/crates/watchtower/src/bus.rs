//! The internal **event bus** (t34, RFD §8): the [`EventBus`] trait (the seam the CF Queues impl
//! backs) + the EC2 [`LocalBus`] (tokio MPSC + a durable in-memory spool for crash-replay).
//!
//! ## At-least-once delivery
//! `publish` enqueues durably; `subscribe` yields events; `ack(id)` removes an event from the
//! spool. An un-acked event STAYS in the spool, so a simulated crash (drop the subscriber without
//! acking) leaves it redeliverable via [`LocalBus::redeliver_unacked`]. This is the at-least-once
//! guarantee the dispatcher relies on (ack only after a successful COMMIT).
//!
//! The trait is in the pure core (wasm-portable); [`LocalBus`] is `native`-gated (it owns the
//! tokio MPSC). The CF Queues impl (a `worker::Queue` producer + the consumer's `MessageBatch`)
//! lands in the deployment ticket behind this same trait.

use crate::event::{Event, EventId};

/// A structured, secret-free bus error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BusError {
    /// The bus is closed (the receiver was dropped) — publish cannot enqueue.
    #[error("event bus is closed")]
    Closed,
    /// The bounded spool is full (back-pressure) — the producer must retry/shed.
    #[error("event bus is full (bounded capacity reached)")]
    Full,
}

/// The event bus seam. `publish` durably enqueues an [`Event`]; `ack` marks one delivered (removes
/// it from the redelivery spool). The EC2 [`LocalBus`] and the (deferred) CF Queues impl both
/// satisfy it, so the dispatcher + producers are runtime-agnostic.
///
/// Deliberately NOT async in the trait surface so the pure core can name it on wasm; the native
/// [`LocalBus`] provides async `subscribe`/recv inherent methods (tokio MPSC) the daemon uses.
pub trait EventBus: Send + Sync {
    /// Durably enqueue `event` (it stays in the redelivery spool until `ack`ed).
    ///
    /// # Errors
    /// [`BusError::Closed`] if no consumer remains; [`BusError::Full`] on back-pressure.
    fn publish(&self, event: Event) -> Result<(), BusError>;

    /// Mark `id` delivered — remove it from the redelivery spool. Acking an unknown id is a
    /// no-op (idempotent, so a duplicate ack after redelivery is harmless).
    ///
    /// # Errors
    /// [`BusError`] only on an internal lock failure (degrades to a dropped ack, never a panic).
    fn ack(&self, id: &EventId) -> Result<(), BusError>;

    /// The number of un-acked events currently in the redelivery spool (the at-least-once
    /// backlog — `0` when everything published has been acked).
    fn unacked_len(&self) -> usize;
}

#[cfg(feature = "native")]
pub use local::LocalBus;

#[cfg(feature = "native")]
mod local {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use tokio::sync::mpsc;

    use super::{BusError, EventBus};
    use crate::event::{Event, EventId};

    /// The EC2 event bus: a bounded tokio MPSC channel for live delivery + a durable in-memory
    /// spool (`BTreeMap<EventId, Event>`) of un-acked events for crash-replay. Bounded capacity
    /// applies back-pressure; an un-acked event is redeliverable after a simulated crash.
    pub struct LocalBus {
        /// The live-delivery sender (the daemon's subscriber holds the receiver).
        tx: mpsc::Sender<Event>,
        /// The durable redelivery spool: every published, not-yet-acked event. The crash-replay
        /// substrate (a `sled`/file spool in a hardened deployment; in-memory here, sufficient for
        /// the recovery test that drops the subscriber without acking).
        spool: Mutex<BTreeMap<EventId, Event>>,
    }

    impl LocalBus {
        /// Construct a bus with a bounded channel of `capacity`, returning the bus + the receiver
        /// the daemon's subscriber loop drains. The bus keeps the durable spool; the receiver is
        /// the live fast-path.
        #[must_use]
        pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Event>) {
            let (tx, rx) = mpsc::channel(capacity.max(1));
            (
                Self {
                    tx,
                    spool: Mutex::new(BTreeMap::new()),
                },
                rx,
            )
        }

        /// Re-enqueue every un-acked event from the spool onto the live channel (the crash-replay
        /// path: after a restart / a dropped subscriber, the un-acked window is redelivered, never
        /// silently skipped). Returns how many were redelivered. Best-effort: a closed channel
        /// stops the replay (the events stay in the spool for the next attempt).
        pub fn redeliver_unacked(&self) -> usize {
            let pending: Vec<Event> = self
                .spool
                .lock()
                .map(|s| s.values().cloned().collect())
                .unwrap_or_default();
            let mut redelivered = 0;
            for event in pending {
                // try_send keeps redelivery non-blocking; a full/closed channel halts the replay.
                if self.tx.try_send(event).is_err() {
                    break;
                }
                redelivered += 1;
            }
            redelivered
        }

        /// A snapshot of the un-acked spool (for the recovery assertion).
        #[must_use]
        pub fn spool_snapshot(&self) -> Vec<Event> {
            self.spool
                .lock()
                .map(|s| s.values().cloned().collect())
                .unwrap_or_default()
        }
    }

    impl EventBus for LocalBus {
        fn publish(&self, event: Event) -> Result<(), BusError> {
            // Durably spool FIRST (so a crash between spool + send still has the event), then
            // attempt the live send. A full channel is back-pressure; the spooled event is
            // redeliverable, so a full live channel does not lose it.
            if let Ok(mut spool) = self.spool.lock() {
                spool.insert(event.id.clone(), event.clone());
            }
            match self.tx.try_send(event) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => Err(BusError::Full),
                Err(mpsc::error::TrySendError::Closed(_)) => Err(BusError::Closed),
            }
        }

        fn ack(&self, id: &EventId) -> Result<(), BusError> {
            if let Ok(mut spool) = self.spool.lock() {
                spool.remove(id);
            }
            Ok(())
        }

        fn unacked_len(&self) -> usize {
            self.spool.lock().map(|s| s.len()).unwrap_or(0)
        }
    }
}
