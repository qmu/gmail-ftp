//! The **`MockHost`** test double (t36 acceptance): an in-memory [`RuntimeHost`] +
//! [`DurableStore`] that records the committed effect set so a test can drive a JOB twice and a
//! WEBHOOK event twice and assert the committed effect is IDENTICAL (at-least-once idempotency,
//! RFD §6).
//!
//! Wasm-clean: the mock carries no tokio. Its async methods are driven by [`block_on`], a tiny
//! dependency-free executor (the futures here never actually suspend — they complete on the first
//! poll — so a no-op waker is sufficient). This keeps the idempotency test runnable in the pure
//! core without pulling an async runtime.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use crate::dto::{BindingSet, Mount, StateBytes, StateKey, Timestamp};
use crate::host::{DurableStore, HostError, HostFuture, NativeStoreHandle, RuntimeHost};

/// One recorded committed effect — the idempotency assertion compares the set of these. A
/// `(cause, run_key)` pair: the JOB/WEBHOOK name + the idempotency key (run-id / event-id) the
/// `cas` cursor guards. A redelivery with the same key must NOT add a second entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommittedEffect {
    /// What fired (`"job:nightly"`, `"webhook:inbound"`).
    pub cause: String,
    /// The idempotency key (run-id / event-id). A redelivery reuses it.
    pub run_key: String,
}

/// An in-memory, dyn-safe [`DurableStore`] (a `BTreeMap` under a `Mutex`). `cas` is real: a
/// redelivery whose `expect` no longer matches loses the race and returns `false`, so the caller
/// treats it as a no-op (the idempotency primitive).
#[derive(Default)]
pub struct MemDurableStore {
    map: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemDurableStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl DurableStore for MemDurableStore {
    fn get<'a>(&'a self, key: &'a StateKey) -> HostFuture<'a, Option<StateBytes>> {
        Box::pin(async move {
            let g = self
                .map
                .lock()
                .map_err(|_| HostError::Durable("mem store poisoned".to_string()))?;
            Ok(g.get(key.as_str()).cloned().map(StateBytes::new))
        })
    }

    fn put<'a>(&'a self, key: &'a StateKey, val: StateBytes) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let mut g = self
                .map
                .lock()
                .map_err(|_| HostError::Durable("mem store poisoned".to_string()))?;
            g.insert(key.as_str().to_string(), val.0);
            Ok(())
        })
    }

    fn cas<'a>(
        &'a self,
        key: &'a StateKey,
        expect: Option<StateBytes>,
        val: StateBytes,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let mut g = self
                .map
                .lock()
                .map_err(|_| HostError::Durable("mem store poisoned".to_string()))?;
            let current = g.get(key.as_str()).cloned();
            let expected = expect.map(|e| e.0);
            if current == expected {
                g.insert(key.as_str().to_string(), val.0);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// The in-memory [`RuntimeHost`] double. Records the deduplicated set of committed effects so a
/// test can prove at-least-once idempotency: driving the same `(cause, run_key)` twice records it
/// ONCE (the second is a `cas`-guarded no-op).
pub struct MockHost {
    now: Timestamp,
    durable: MemDurableStore,
    committed: RefCell<Vec<CommittedEffect>>,
}

impl MockHost {
    /// A fresh mock at the given clock time.
    #[must_use]
    pub fn new(now: Timestamp) -> Self {
        Self {
            now,
            durable: MemDurableStore::new(),
            committed: RefCell::new(Vec::new()),
        }
    }

    /// Drive a committed effect through the idempotency gate: `cas` the cause's cursor from its
    /// prior value to `run_key`. Records the effect ONLY on a successful swap (first delivery); a
    /// redelivery with the same key finds the cursor already at `run_key` and is a no-op
    /// (at-least-once, RFD §6). Returns whether the effect was newly committed.
    pub fn deliver(&self, cause: &str, run_key: &str) -> Result<bool, HostError> {
        let key = StateKey::new(format!("cursor/{cause}"));
        let prior = block_on(self.durable.get(&key))?;
        let prior_key = prior
            .as_ref()
            .map(|b| String::from_utf8_lossy(b.as_slice()).into_owned());
        // Already at this run_key ⇒ a redelivery ⇒ no-op.
        if prior_key.as_deref() == Some(run_key) {
            return Ok(false);
        }
        let swapped = block_on(self.durable.cas(
            &key,
            prior,
            StateBytes::new(run_key.as_bytes().to_vec()),
        ))?;
        if swapped {
            self.committed.borrow_mut().push(CommittedEffect {
                cause: cause.to_string(),
                run_key: run_key.to_string(),
            });
        }
        Ok(swapped)
    }

    /// The deduplicated, sorted set of committed effects (the idempotency assertion compares this).
    #[must_use]
    pub fn committed_effects(&self) -> Vec<CommittedEffect> {
        let mut v = self.committed.borrow().clone();
        v.sort();
        v.dedup();
        v
    }
}

impl RuntimeHost for MockHost {
    fn now(&self) -> Timestamp {
        self.now
    }

    async fn serve_endpoints(&self, _set: &BindingSet) -> Result<(), HostError> {
        Ok(())
    }

    async fn schedule_jobs(&self, _set: &BindingSet) -> Result<(), HostError> {
        Ok(())
    }

    async fn consume_events(&self, _set: &BindingSet) -> Result<(), HostError> {
        Ok(())
    }

    fn durable(&self) -> &dyn DurableStore {
        &self.durable
    }

    fn native_store(&self, set: &BindingSet, mount: &Mount) -> Option<NativeStoreHandle> {
        set.native_stores
            .iter()
            .find(|ns| &ns.mount == mount)
            .map(|ns| NativeStoreHandle {
                mount: ns.mount.clone(),
                binding_name: ns.binding_name(),
            })
    }
}

/// A tiny dependency-free, `unsafe`-free `block_on`: box-pin the future and poll it to completion
/// with the std no-op waker ([`Waker::noop`], stable since 1.85). The mock's `DurableStore`
/// futures never suspend (they complete on the first poll), so this returns the first `Ready`
/// without ever busy-looping. Used so the wasm-clean core can drive its async `DurableStore` in a
/// test without pulling an async runtime. A pending poll yields the OS thread (a guard against an
/// accidental never-completing future), but the mock's futures are always immediately ready.
#[must_use]
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut boxed: Pin<Box<F>> = Box::pin(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match boxed.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_least_once_redelivery_is_idempotent() {
        let host = MockHost::new(Timestamp::from_secs(1000));
        // JOB nightly fires twice for the same scheduled run (run-id is deterministic).
        assert!(host.deliver("job:nightly", "run-2026-06-23T00").unwrap());
        assert!(!host.deliver("job:nightly", "run-2026-06-23T00").unwrap());
        // WEBHOOK inbound event delivered twice (same event-id).
        assert!(host.deliver("webhook:inbound", "evt-abc").unwrap());
        assert!(!host.deliver("webhook:inbound", "evt-abc").unwrap());

        let effects = host.committed_effects();
        assert_eq!(
            effects.len(),
            2,
            "two distinct causes, each committed exactly once despite redelivery: {effects:?}"
        );
    }
}
