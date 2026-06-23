#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! # `cfs-host` — the deployment host-adapter seam (t36, RFD-0001 §8/§9)
//!
//! "The runtime is just what causes a plan to run" (RFD §2). This crate is the ONE boundary that
//! abstracts *what causes a plan to run* over the two production targets — a long-lived **Linux/EC2
//! daemon** and a **`wasm32` Cloudflare Worker** — without inventing any new runtime semantics. The
//! closed-core grammar, the three `/server` registries (t30), the binding model, and the three
//! fire-path leaf bindings (`cfs-http`/`cfs-cron`/`cfs-watchtower`) are all untouched; t36 takes the
//! already-built effect-plan interpreter + server registry and runs them on each target.
//!
//! ## The seam
//! - [`RuntimeHost`] — `now` / `serve_endpoints` / `schedule_jobs` / `consume_events` / `durable` /
//!   `native_store`: the cause-attachment contract each host implements.
//! - [`DurableStore`] — `get` / `put` / `cas`: watcher cursors + `LAST_RUN` over DO storage (CF) or
//!   an fsync'd file (daemon); `cas` is the at-least-once idempotency primitive (RFD §6).
//! - Owned, vendor-free DTOs: [`BindingSet`] (+ [`EndpointBinding`], [`JobBinding`],
//!   [`WebhookBinding`], [`WatcherBinding`], [`NativeStoreBinding`], [`Mount`],
//!   [`NativeStoreHandle`], [`Timestamp`], [`StateKey`], [`StateBytes`]). Produced by the t30
//!   registry, consumed identically by both hosts. NO `worker::*` / `tokio::*` / AWS / CF storage
//!   type ever crosses this seam (enforced by the cargo-metadata no-vendor-in-core deny test).
//!
//! ## What builds where (the two offline-dep constraints, ADR-0005)
//! - The **wasm-clean core** (this crate with `--no-default-features`): the traits, the DTOs, the
//!   host-agnostic binding-set [`derive`]ation, the [`wrangler`] generator, and the [`mock`]
//!   `MockHost`. Builds on `wasm32-unknown-unknown` — pulls NO tokio / worker.
//! - **`host-daemon`**: the EC2 side — [`bindings_from_state`] (the `cfs_server::ServerState` →
//!   [`BindingSet`] conversion; `cfs-server` pulls tokio `signal`, no-wasm), the fsync'd
//!   [`FileDurableStore`], and the on-disk [`AuditLedger`]. The daemon's `TokioHost: RuntimeHost`
//!   is composed in the terminal `cfs` binary, REUSING the existing `cfs-http`/`cfs-cron`/
//!   `cfs-watchtower` serve composition behind the trait — it is NOT rebuilt here.
//! - **`host-workers`**: the CF side — PARKED. The `worker` crate is not in the offline cache, so
//!   the real `#[event]` + `#[durable_object]` entrypoints are scaffolded (the
//!   archetype→primitive [`workers`] mapping + DTO wiring), drop-in once `worker` lands.
//!
//! The two host features are mutually exclusive in a single build (a `compile_error!` fires if
//! both are set — see below).

#[cfg(all(feature = "host-daemon", feature = "host-workers"))]
compile_error!(
    "cfs-host: `host-daemon` and `host-workers` are mutually exclusive in a single build (one \
     deployment target per binary). Enable exactly one (or neither for the wasm-clean core)."
);

pub mod derive;
pub mod dto;
pub mod host;
pub mod mock;
pub mod wrangler;

#[cfg(feature = "host-daemon")]
pub mod daemon;
#[cfg(feature = "host-daemon")]
pub mod from_server;

#[cfg(feature = "host-workers")]
pub mod workers;

// --- The seam (always present) ---
pub use derive::{
    cron_from_every, derive_bindings, DerivationInput, EndpointInput, JobInput, WatcherInput,
    WebhookInput,
};
pub use dto::{
    BindingSet, EndpointBinding, JobBinding, Mount, NativeStoreBinding, NativeStoreKind,
    StateBytes, StateKey, Timestamp, WatcherBinding, WebhookBinding,
};
pub use host::{DurableStore, HostError, HostFuture, NativeStoreHandle, RuntimeHost};
pub use mock::{block_on, CommittedEffect, MemDurableStore, MockHost};
pub use wrangler::{generate_wrangler_toml, DURABLE_OBJECT_CLASS, WORKER_MODULE};

// --- Daemon side ---
#[cfg(feature = "host-daemon")]
pub use daemon::{AuditLedger, FileDurableStore};
#[cfg(feature = "host-daemon")]
pub use from_server::{bindings_from_config, bindings_from_state, derivation_input};

// --- Workers side (parked) ---
#[cfg(feature = "host-workers")]
pub use workers::{
    endpoint_event, job_event, native_store_handle, resolve_native_store, watcher_event, CfEvent,
    DurableObjectStore,
};
