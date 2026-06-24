//! The **`RuntimeHost` seam** (t36, RFD-0001 §8): "the runtime is just what causes a plan to run"
//! — abstracted over the EC2 daemon and the Cloudflare Worker.
//!
//! This is the ONE boundary. Both hosts produce the identical owned [`crate::BindingSet`] from the
//! t30 registry and attach causes to it: the daemon a `tokio` listener + interval + bus, the
//! Worker the `fetch`/`scheduled`/`queue` handlers + a Durable Object. No `tokio::*`/`worker::*`
//! symbol appears in this trait's surface — only owned DTOs and a `HostError`.
//!
//! ## async-fn-in-trait, no `async-trait` crate
//! [`RuntimeHost`] uses native `async fn` in traits (stable since Rust 1.75; this workspace pins
//! 1.96) so the wasm-clean core pulls NO `async-trait`/`tokio` dependency. A `RuntimeHost` is used
//! as a concrete type by each host's composition root, never as `dyn RuntimeHost`, so AFIT's
//! non-dyn-safety is not a constraint here.
//!
//! ## `DurableStore` is dyn-safe by design
//! [`RuntimeHost::durable`] returns `&dyn DurableStore`, so [`DurableStore`] must be object-safe.
//! Its `get/put/cas` therefore return `Pin<Box<dyn Future>>` (the hand-rolled equivalent of an
//! `#[async_trait]` method) rather than `async fn`, keeping it usable behind a trait object while
//! the wasm core still avoids the `async-trait` macro crate.

use std::future::Future;
use std::pin::Pin;

use crate::dto::{BindingSet, Mount, StateBytes, StateKey, Timestamp};

/// The structured, secret-free host error. Every variant carries a NAME / reason, never a token
/// or a credential-bearing value (RFD §10) — it is safe to log and to surface to an operator.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// A durable-state operation failed (read/write/cas), with a secret-free reason.
    #[error("durable store: {0}")]
    Durable(String),
    /// A cause could not be attached (a bind failed, a handler could not register), secret-free.
    #[error("attach: {0}")]
    Attach(String),
    /// A native-store binding was requested for an unknown / unbound mount.
    #[error("no native store bound for mount {0}")]
    UnboundMount(String),
}

/// A boxed future result — the dyn-safe async return for [`DurableStore`]. Not `Send` so a CF
/// Durable Object's single-threaded `!Send` futures are expressible; the daemon's fsync'd store is
/// trivially `!Send`-compatible.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HostError>> + 'a>>;

/// Owned KV-ish durable state over a CF Durable Object's storage (CF) or an fsync'd file (daemon)
/// — the watcher cursors + `LAST_RUN` high-water marks (RFD §8). `cas` is the at-least-once /
/// idempotency primitive (RFD §6): a redelivered job/event advances the cursor only if it still
/// holds the expected prior value, so a redelivery is a no-op.
///
/// Object-safe: `RuntimeHost::durable` hands out `&dyn DurableStore`. The methods return a boxed
/// future ([`HostFuture`]) rather than `async fn` so the trait stays dyn-safe with no
/// `async-trait` dependency in the wasm-clean core.
pub trait DurableStore {
    /// Read the value at `key`, or `None` if unset.
    fn get<'a>(&'a self, key: &'a StateKey) -> HostFuture<'a, Option<StateBytes>>;

    /// Write `val` at `key` (last-writer-wins). Durable before the future resolves (fsync on the
    /// daemon; the DO storage write on CF).
    fn put<'a>(&'a self, key: &'a StateKey, val: StateBytes) -> HostFuture<'a, ()>;

    /// Compare-and-set: write `val` only if the current value equals `expect`. Returns `true` on a
    /// successful swap, `false` if the current value did not match (the caller lost the race / the
    /// redelivery is a no-op). The idempotency primitive (RFD §6).
    fn cas<'a>(
        &'a self,
        key: &'a StateKey,
        expect: Option<StateBytes>,
        val: StateBytes,
    ) -> HostFuture<'a, bool>;
}

/// What causes a plan to run, abstracted over EC2 vs Workers (RFD §8). A host attaches the owned
/// [`BindingSet`]'s causes to its platform: `serve_endpoints` (→ `fetch`/route), `schedule_jobs`
/// (→ Cron Trigger/interval), `consume_events` (→ Queue/bus), `durable` (→ DO storage/file), and
/// `native_store` (→ `env.d1()/.bucket()/.kv()`/HTTP client). The single effect-plan interpreter
/// runs unchanged on top — the host never bypasses it (the purity invariant, RFD §3/§6).
pub trait RuntimeHost {
    /// The host clock (RFD §6: wasm has no `SystemClock`; the daemon reads the system clock, the
    /// Worker reads the request/event time). Synchronous — `now` is a pure read.
    fn now(&self) -> Timestamp;

    /// Attach the ENDPOINT causes (→ a `fetch` handler on CF, a route table on the daemon).
    ///
    /// # Errors
    /// [`HostError::Attach`] if the cause could not be attached (e.g. a port bind failed).
    fn serve_endpoints(&self, set: &BindingSet) -> impl Future<Output = Result<(), HostError>>;

    /// Attach the JOB causes (→ Cron Triggers on CF, a `tokio::time` interval on the daemon).
    ///
    /// # Errors
    /// [`HostError::Attach`] if the schedule could not be installed.
    fn schedule_jobs(&self, set: &BindingSet) -> impl Future<Output = Result<(), HostError>>;

    /// Attach the WEBHOOK/event causes (→ a Queue consumer on CF, the in-process bus + `/hooks/...`
    /// ingest on the daemon).
    ///
    /// # Errors
    /// [`HostError::Attach`] if the consumer could not be attached.
    fn consume_events(&self, set: &BindingSet) -> impl Future<Output = Result<(), HostError>>;

    /// The durable store for watcher cursors / `LAST_RUN` (→ DO storage on CF, an fsync'd file on
    /// the daemon).
    fn durable(&self) -> &dyn DurableStore;

    /// The native store backing a `/cf/d1`·`/cf/r2`·`/cf/kv` mount (→ an `env` binding on CF, the
    /// driver's HTTP client on the daemon). `None` if the mount is not bound by this deployment.
    fn native_store(&self, set: &BindingSet, mount: &Mount) -> Option<NativeStoreHandle>;
}

/// An opaque handle to a bound native store (RFD §8). Owned + vendor-free: on CF it names the
/// `env` binding (`env.d1(name)`), on the daemon it names the driver's HTTP client. The handle
/// carries the binding NAME only — never a token (RFD §10). The concrete backend is resolved by
/// the host; consumers thread the handle to the driver, which knows how to use it on its platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStoreHandle {
    /// The mount this handle backs (`/cf/d1/<db>` etc).
    pub mount: Mount,
    /// The `env` binding name the Worker references (`env.<binding_name>`), or the daemon's
    /// driver-client key. Name-only.
    pub binding_name: String,
}
