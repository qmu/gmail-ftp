//! The **Cloudflare Workers** host (behind `host-workers`): PARKED per docs/adr/0005.
//!
//! ## Why this is a scaffold, not the real `worker` entrypoints
//! The `worker` crate (the CF Workers Rust SDK) is **not in the offline cache** — it is not even
//! resolvable from the offline crates.io index on this host (verified: `cargo` reports "no
//! matching package named `worker`" under `--offline`). Per the ADR-0002/0003/0004 footprint
//! reasoning (an uncached heavy dependency tree on a ~99 %-full disk is exactly the risk those
//! ADRs were written to avoid), the real `worker`-crate `#[event(fetch/scheduled/queue)]` +
//! `#[durable_object]` entrypoints are **parked behind this feature** and documented in
//! `docs/adr/0005-deployment-hosts.md`. What ships now is the wasm-clean SCAFFOLD: the
//! binding-archetype → CF-primitive MAPPING and the DTO wiring, so the real entrypoints are
//! drop-in once `worker` lands.
//!
//! This module pulls NO `worker` dependency. It builds for `wasm32-unknown-unknown` (it is owned
//! DTOs + the mapping table only). The functions below describe the mapping each real `#[event]`
//! handler will implement; they are the contract a reviewer checks the eventual handlers against.

use crate::dto::{
    BindingSet, EndpointBinding, JobBinding, Mount, NativeStoreBinding, WatcherBinding,
    WebhookBinding,
};
use crate::host::{DurableStore, HostError, HostFuture, NativeStoreHandle};
use crate::wrangler::DURABLE_OBJECT_CLASS;

/// The four CF event archetypes a [`BindingSet`] maps onto (RFD §8). The real Worker exposes one
/// `#[event(...)]` handler per archetype; this enum is the parked mapping the scaffold pins so the
/// eventual handlers are a mechanical fill-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CfEvent {
    /// `#[event(fetch)]` — serves ENDPOINT causes (one `fetch` handler routes every endpoint).
    Fetch,
    /// `#[event(scheduled)]` — fires JOB causes matched by the firing Cron Trigger's `cron` expr.
    Scheduled,
    /// `#[event(queue)]` — consumes WEBHOOK/event causes from the bound Queue.
    Queue,
    /// The `#[durable_object]` class — backs watcher cursors / `LAST_RUN` via [`DurableStore`].
    DurableObject,
}

/// Which CF event archetype an ENDPOINT binding maps onto (→ `fetch`). The Worker's single
/// `fetch` handler matches `req.method`/`req.url` against the [`EndpointBinding`] route table the
/// (parked) entrypoint reconciles from the deployed config.
#[must_use]
pub fn endpoint_event(_ep: &EndpointBinding) -> CfEvent {
    CfEvent::Fetch
}

/// Which CF event archetype a JOB binding maps onto (→ `scheduled`/Cron). The Worker's
/// `#[event(scheduled)]` handler receives the firing `cron` string and matches it against the
/// [`JobBinding::cron`] of each job (a redelivery is idempotent via the `cas`-guarded cursor).
#[must_use]
pub fn job_event(_job: &JobBinding) -> CfEvent {
    CfEvent::Scheduled
}

/// Which CF event archetype a WEBHOOK binding maps onto (→ `queue`). A `/hooks/...` `fetch`
/// publishes to the [`WebhookBinding::queue`]; the `#[event(queue)]` handler drains it (the
/// Queue's at-least-once delivery is made idempotent by the dispatcher's `cas` dedup).
#[must_use]
pub fn webhook_event(_wh: &WebhookBinding) -> CfEvent {
    CfEvent::Queue
}

/// Which CF event archetype a watcher binding's durable state maps onto (→ the Durable Object).
/// The [`WatcherBinding::cursor_key`] is the DO storage key; the DO class is
/// [`DURABLE_OBJECT_CLASS`]. The `#[durable_object]` struct implements [`DurableStore`] over its
/// single-threaded storage (RFD §8: DO single-threaded concurrency for `LAST_RUN`).
#[must_use]
pub fn watcher_event(_w: &WatcherBinding) -> (CfEvent, &'static str) {
    (CfEvent::DurableObject, DURABLE_OBJECT_CLASS)
}

/// The `env` binding name a native-store mount resolves to on the Worker (`env.<binding_name>`).
/// On CF this is `env.d1(name)` / `env.bucket(name)` / `env.kv(name)`; the real `native_store`
/// returns a [`NativeStoreHandle`] wrapping the live binding. PARKED: today it returns the
/// name-only handle (no `worker` env to resolve against).
#[must_use]
pub fn native_store_handle(ns: &NativeStoreBinding) -> NativeStoreHandle {
    NativeStoreHandle {
        mount: ns.mount.clone(),
        binding_name: ns.binding_name(),
    }
}

/// Resolve a native-store handle for `mount` from the binding set (the parked `WorkersHost`'s
/// `native_store` body). `None` for an unbound mount.
#[must_use]
pub fn resolve_native_store(set: &BindingSet, mount: &Mount) -> Option<NativeStoreHandle> {
    set.native_stores
        .iter()
        .find(|ns| &ns.mount == mount)
        .map(native_store_handle)
}

/// The parked Durable-Object-backed [`DurableStore`]. On CF the `#[durable_object]` struct holds a
/// `worker::State` whose `storage()` is the durable KV; `get/put/cas` map to its
/// `get/put/transaction`. Until `worker` lands this scaffold returns a structured error on use, so
/// a caller that reaches it under `host-workers` fails CLEANLY (never a panic) — the contract is
/// pinned, the backend is parked.
#[derive(Default)]
pub struct DurableObjectStore;

impl DurableObjectStore {
    /// A fresh (parked) DO-backed store.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl DurableStore for DurableObjectStore {
    fn get<'a>(
        &'a self,
        _key: &'a crate::dto::StateKey,
    ) -> HostFuture<'a, Option<crate::dto::StateBytes>> {
        Box::pin(async move {
            Err(HostError::Durable(
                "host-workers DurableObjectStore parked: requires the `worker` crate (ADR-0005)"
                    .to_string(),
            ))
        })
    }

    fn put<'a>(
        &'a self,
        _key: &'a crate::dto::StateKey,
        _val: crate::dto::StateBytes,
    ) -> HostFuture<'a, ()> {
        Box::pin(async move {
            Err(HostError::Durable(
                "host-workers DurableObjectStore parked: requires the `worker` crate (ADR-0005)"
                    .to_string(),
            ))
        })
    }

    fn cas<'a>(
        &'a self,
        _key: &'a crate::dto::StateKey,
        _expect: Option<crate::dto::StateBytes>,
        _val: crate::dto::StateBytes,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            Err(HostError::Durable(
                "host-workers DurableObjectStore parked: requires the `worker` crate (ADR-0005)"
                    .to_string(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::NativeStoreKind;

    #[test]
    fn archetype_mapping_is_pinned() {
        let ep = EndpointBinding {
            name: "recent".to_string(),
            method: "GET".to_string(),
            route: "/recent".to_string(),
            policy: None,
        };
        assert_eq!(endpoint_event(&ep), CfEvent::Fetch);

        let job = JobBinding {
            name: "nightly".to_string(),
            every: "1h".to_string(),
            cron: "0 * * * *".to_string(),
            policy: None,
        };
        assert_eq!(job_event(&job), CfEvent::Scheduled);

        let wh = WebhookBinding {
            name: "inbound".to_string(),
            route: "/hooks/x".to_string(),
            secret_handle: String::new(),
            queue: "inbound-events".to_string(),
        };
        assert_eq!(webhook_event(&wh), CfEvent::Queue);

        let w = WatcherBinding {
            name: "notify".to_string(),
            on: "inbox".to_string(),
            policy: None,
        };
        assert_eq!(
            watcher_event(&w),
            (CfEvent::DurableObject, DURABLE_OBJECT_CLASS)
        );

        let ns = NativeStoreBinding {
            kind: NativeStoreKind::D1,
            resource: "analytics".to_string(),
            mount: Mount::new("/cf/d1/analytics"),
        };
        assert_eq!(native_store_handle(&ns).binding_name, "D1_ANALYTICS");
    }
}
