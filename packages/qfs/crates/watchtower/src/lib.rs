//! # qfs-watchtower (t34, RFD-0001 §8)
//!
//! The **watchtower**: the "cause" side of the server runtime that turns external change into
//! fired effect-plans. Two cause sources — inbound **WEBHOOKs** ([`webhook`], HMAC-verified
//! ingress) and polling source **WATCHERs** ([`watcher`], cursor-diff over the read path) — emit
//! normalized owned [`Event`](event::Event)s onto an internal [`EventBus`](bus::EventBus);
//! registered **TRIGGERs** ([`dispatch`]) match an event, evaluate the optional `WHERE` over
//! `NEW.*` ([`predicate`]), bind `NEW.*` into the handler plan ([`bind`]), pass the policy gate
//! hook, and `COMMIT` through an INJECTED [`Committer`](commit::Committer).
//!
//! ## At-least-once + idempotency (RFD §6)
//! Delivery is at-least-once: ack ONLY after a successful COMMIT; the [`bus::LocalBus`] durable
//! spool redelivers an un-acked event after a crash; a [`dedup_key`](event::Event::dedup_key)
//! carried end-to-end + the dispatcher's idempotency ledger make a re-delivered event a no-op after
//! its first net effect. Non-idempotent procs (`CALL mail.send`) still need an explicit dedupe
//! guard in the plan.
//!
//! ## Topology (a LEAF, the watchtower sibling of qfs-http/qfs-cron)
//! Consumed ONLY by the `qfs` binary (the serve composition root). It depends on qfs-server +
//! qfs-exec (a leaf integration consumer of the read path, the role qfs-cmd/qfs-http/qfs-cron play)
//! but NOT on qfs-runtime — the real commit path is the INJECTED [`Committer`](commit::Committer),
//! so the qfs-exec-consumer + runtime-leaf confinement guards stay green and its feature-gated
//! tokio dead-ends in the terminal binary. It also depends on NEITHER qfs-http NOR a vendor HTTP
//! type: the [`WebhookBinding::ingest`](webhook::WebhookBinding::ingest) is a pure handler over
//! owned request data the BINARY composes into the qfs-http listener.
//!
//! ## Purity / wasm
//! The PURE core ([`event`], [`predicate`], [`bind`], the [`bus::EventBus`]/[`watcher::WatcherStore`]/
//! [`commit::Committer`]/[`commit::PolicyGate`] traits, the dedup logic) has ZERO tokio and builds
//! for `wasm32-unknown-unknown` with `--no-default-features` (the CF Queues/DO mapping). The native
//! [`bus::LocalBus`] + [`watcher::Watcher::poll_once`] + the [`Committer`](commit::RecordingCommitter)
//! are behind the default-on `native` feature (the t25/t33 absence-of-`native` wasm fence).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod bind;
pub mod bus;
pub mod commit;
pub mod event;
pub mod predicate;
pub mod watcher;

// The server-coupled pieces (the Binding impls) need qfs-server + the dispatcher needs qfs-exec via
// the committer — both behind `native`. The pure core above builds on wasm32 without them.
#[cfg(feature = "native")]
pub mod binding;
#[cfg(feature = "native")]
pub mod dispatch;
#[cfg(feature = "native")]
pub mod webhook;

// ---- Pure-core re-exports (available on every target) ----
pub use bind::{bind_new, NewBindings};
pub use bus::{BusError, EventBus};
pub use commit::{AllowAllGate, Committer, FireError, FireOutcome, PolicyGate};
pub use event::{Event, EventId, EventKind, SourcePath};
pub use predicate::{guard_matches, GuardError};
pub use watcher::{MemWatcherStore, Watcher, WatcherCursor, WatcherStore};

// The parser Statement the injected Committer commits + the qfs-server types the binary's
// composition root needs (so the binary does NOT take a direct qfs-server dep — which the
// dep-direction guard forbids for the terminal binary; it reaches them through this leaf).
pub use qfs_parser::Statement;
#[cfg(feature = "native")]
pub use qfs_server::{
    AuditEntry, AuditSink, Binding, FiredDecision, FiredPlanRecord, PolicyTable, TriggerDef,
};

// ---- Native re-exports ----
#[cfg(feature = "native")]
pub use binding::{PolicyTableHandle, WatcherSet, WatchtowerBinding};
#[cfg(feature = "native")]
pub use bus::LocalBus;
#[cfg(feature = "native")]
pub use commit::RecordingCommitter;
#[cfg(feature = "native")]
pub use dispatch::{Dispatched, Dispatcher};
#[cfg(feature = "native")]
pub use webhook::{
    sign_body, IngestOutcome, WebhookBinding, WebhookIngest, WebhookRoutes, SIGNATURE_HEADER,
};

#[cfg(test)]
mod tests;
