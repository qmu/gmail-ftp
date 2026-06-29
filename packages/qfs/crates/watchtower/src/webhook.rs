//! **Inbound webhook ingestion** (t34, RFD §8/§10): [`WebhookBinding`] implements
//! [`qfs_server::Binding`] (kind `Ingest`) — `reconcile` rebuilds the `/hooks/...` route set from
//! the `WebhookDef`s; [`WebhookBinding::ingest`] verifies the per-webhook signing secret (resolved
//! BY HANDLE from `qfs-secrets`, NEVER inlined / logged) via `qfs-crypto-core` HMAC + a
//! constant-time compare, durably enqueues ONE [`Event`], then returns 2xx (ack-after-enqueue).
//! An invalid signature → 401, enqueues NONE.
//!
//! ## Webhook serving topology (the binary composes; qfs-watchtower serves no HTTP)
//! [`WebhookBinding::ingest`] is a PURE async handler over owned request data (route + headers +
//! body) returning an [`IngestOutcome`]. qfs-watchtower depends on NEITHER qfs-http NOR any vendor
//! HTTP type — the `qfs` binary (the serve composition root) wires this ingest into the qfs-http
//! listener for `/hooks/...` paths, so the two leaves cross only through owned DTOs + a closure.
//! A CF Worker `fetch` maps the same `ingest` onto a Workers Request unchanged.
//!
//! ## Signature scheme
//! `X-Cfs-Signature: v0=<lowercase-hex HMAC-SHA256(secret, body)>` (the Slack-shaped scheme over
//! the shared qfs-crypto-core primitive). An UNSIGNED webhook (empty secret handle) accepts without
//! a check (a documented less-secure mode for an internal/test ingress; signed is production).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secrets};
use qfs_server::{Binding, BindingKind, ServerError, ServerState};

use crate::bus::EventBus;
use crate::event::{Event, EventKind, SourcePath};

/// The HTTP header carrying the webhook signature (`v0=<hex>`).
pub const SIGNATURE_HEADER: &str = "x-qfs-signature";
/// The `qfs-secrets` driver namespace webhook signing secrets live under (RFD §10 — resolved by
/// handle; the account id is the per-webhook `secret` handle from the `WebhookDef`).
pub const WEBHOOK_SECRET_DRIVER: &str = "webhook";
/// The `v0=` signature version prefix.
const SIG_VERSION: &str = "v0";

/// The outcome of an ingest: the HTTP status to return + whether an event was published. Owned,
/// vendor-free — the binary maps it onto an `HttpResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    /// The HTTP status code (`202` accepted+enqueued, `401` bad signature, `404` no route,
    /// `500` resolution/publish failure).
    pub status: u16,
    /// Whether exactly one event was durably enqueued (true only on `202`).
    pub published: bool,
}

impl IngestOutcome {
    /// A 202-accepted outcome (signature verified, one event enqueued).
    #[must_use]
    pub fn accepted() -> Self {
        Self {
            status: 202,
            published: true,
        }
    }

    /// A 401-unauthorized outcome (bad/missing signature; zero events).
    #[must_use]
    pub fn unauthorized() -> Self {
        Self {
            status: 401,
            published: false,
        }
    }

    /// A 404 outcome (no webhook reconciled for the route; zero events).
    #[must_use]
    pub fn not_found() -> Self {
        Self {
            status: 404,
            published: false,
        }
    }

    /// A 500 outcome (secret resolution / publish failure; zero events).
    #[must_use]
    pub fn internal() -> Self {
        Self {
            status: 500,
            published: false,
        }
    }
}

/// The reconciled webhook route set: route path → (name, signing-secret handle). Immutable once
/// built (the binding swaps the `Arc` pointer atomically), so an in-flight ingest holds a
/// consistent snapshot.
#[derive(Debug, Default)]
pub struct WebhookRoutes {
    routes: BTreeMap<String, WebhookRoute>,
}

/// One reconciled route: the webhook name + its signing-secret HANDLE (never the secret itself).
#[derive(Debug, Clone)]
struct WebhookRoute {
    name: String,
    /// The signing-secret handle (a `qfs-secrets` account id). Empty = unsigned (test/internal).
    secret_handle: String,
}

impl WebhookRoutes {
    fn from_state(state: &ServerState) -> Self {
        let mut routes = BTreeMap::new();
        for def in state.webhooks.values() {
            if def.route.is_empty() {
                continue; // a declared-but-empty webhook has no live route
            }
            routes.insert(
                def.route.clone(),
                WebhookRoute {
                    name: def.name.clone(),
                    secret_handle: def.secret.clone(),
                },
            );
        }
        Self { routes }
    }

    /// The number of live routes (test/observability aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the route set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    fn get(&self, route: &str) -> Option<&WebhookRoute> {
        self.routes.get(route)
    }
}

/// The shared **ingest core**: the secrets surface, the event bus, and the atomically-swappable
/// route set. Held behind an `Arc` so BOTH the [`WebhookBinding`] (which `reconcile`s the routes)
/// and the binary's HTTP fallback closure (which calls [`WebhookIngest::ingest`]) share ONE
/// instance — that is how the binary composes the ingest into the qfs-http listener (option b)
/// without the binding being owned by both the runtime and the listener.
pub struct WebhookIngest {
    secrets: Arc<dyn Secrets>,
    bus: Arc<dyn EventBus>,
    routes: Arc<RwLock<Arc<WebhookRoutes>>>,
}

impl WebhookIngest {
    /// Construct the ingest core over a shared secrets surface + event bus (empty route set).
    #[must_use]
    pub fn new(secrets: Arc<dyn Secrets>, bus: Arc<dyn EventBus>) -> Self {
        Self {
            secrets,
            bus,
            routes: Arc::new(RwLock::new(Arc::new(WebhookRoutes::default()))),
        }
    }

    /// A shared handle to the live route set.
    #[must_use]
    pub fn routes_handle(&self) -> Arc<RwLock<Arc<WebhookRoutes>>> {
        Arc::clone(&self.routes)
    }

    /// Snapshot the current live route set (clones the `Arc`; the guard is dropped immediately).
    #[must_use]
    pub fn current_routes(&self) -> Arc<WebhookRoutes> {
        self.routes
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(WebhookRoutes::default()))
    }

    /// Converge the route set to `state` (the binding delegates its `reconcile` here).
    fn reconcile_routes(&self, state: &ServerState) -> Result<(), ServerError> {
        let new_routes = Arc::new(WebhookRoutes::from_state(state));
        let count = new_routes.len();
        if let Ok(mut guard) = self.routes.write() {
            *guard = new_routes;
        } else {
            return Err(ServerError::Reconcile {
                kind: BindingKind::Ingest.label().to_string(),
                reason: "webhook route table lock poisoned".to_string(),
            });
        }
        tracing::info!(
            target: "qfs::watchtower",
            routes = count,
            webhooks = state.webhooks.len(),
            "webhook route table reconciled"
        );
        Ok(())
    }

    /// **Ingest** an inbound webhook request (the pure handler the binary's listener calls). Looks
    /// up the route, verifies the `X-Cfs-Signature` HMAC against the per-webhook signing secret
    /// (resolved BY HANDLE from `qfs-secrets`, NEVER logged), durably enqueues ONE [`Event`], and
    /// returns 202. An invalid/missing signature → 401, enqueues NONE. Ack-after-enqueue: the 2xx
    /// is returned only AFTER the durable `publish` (at-least-once).
    ///
    /// `now` is the epoch-second receipt time (the caller supplies the clock — the handler is pure
    /// w.r.t. wall-clock).
    pub fn ingest(
        &self,
        route: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        now: i64,
    ) -> IngestOutcome {
        let routes = self.current_routes();
        let webhook = match routes.get(route) {
            Some(w) => w.clone(),
            None => return IngestOutcome::not_found(),
        };

        // Signature verification (RFD §10 replay defense). An empty handle = unsigned (accepted
        // without a check — the documented less-secure internal mode); a non-empty handle REQUIRES
        // a valid signature.
        if !webhook.secret_handle.is_empty()
            && !self.verify_signature(&webhook.secret_handle, headers, body)
        {
            tracing::warn!(
                target: "qfs::watchtower",
                webhook = %webhook.name,
                "webhook signature verification FAILED; rejecting (401), enqueuing nothing"
            );
            return IngestOutcome::unauthorized();
        }

        // Build the normalized Event. The NEW.* payload is the raw body as a single `body` text
        // field (a richer JSON-field mapping is the per-source decode carry-over); the native id is
        // a content hash of the body so a re-delivered identical request shares a dedup_key.
        let body_text = String::from_utf8_lossy(body).into_owned();
        let native_id = qfs_crypto_core::sha256_hex(body);
        let columns = vec!["body".to_string()];
        let row = qfs_core::Row::new(vec![qfs_core::Value::Text(body_text)]);
        let event = Event::new(
            format!("{route}#{native_id}"),
            SourcePath::new(route.to_string()),
            EventKind::Webhook,
            &native_id,
            columns,
            row,
            now,
        );

        // Durably enqueue, THEN return 2xx (ack-after-enqueue, at-least-once).
        match self.bus.publish(event) {
            Ok(()) => {
                tracing::info!(
                    target: "qfs::watchtower",
                    webhook = %webhook.name,
                    "webhook accepted; one event enqueued"
                );
                IngestOutcome::accepted()
            }
            Err(e) => {
                tracing::warn!(
                    target: "qfs::watchtower",
                    webhook = %webhook.name,
                    error = %e,
                    "webhook enqueue failed; returning 500 (not acked)"
                );
                IngestOutcome::internal()
            }
        }
    }

    /// Verify `X-Cfs-Signature: v0=<hex>` against `HMAC-SHA256(secret, body)` with a constant-time
    /// compare. The secret is resolved BY HANDLE and exposed only inside this function; it never
    /// enters a log, an error, or the Event. A missing header / malformed prefix / unresolvable
    /// secret all verify FALSE (fail-closed).
    fn verify_signature(
        &self,
        secret_handle: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> bool {
        let provided = match headers.get(SIGNATURE_HEADER) {
            Some(v) => v,
            None => return false,
        };
        let provided_hex = match provided.strip_prefix(&format!("{SIG_VERSION}=")) {
            Some(h) => h,
            None => return false,
        };
        // Resolve the signing secret BY HANDLE (never inlined). A resolution failure fails closed.
        let account = match ConnectionId::new(secret_handle) {
            Ok(a) => a,
            Err(_) => return false,
        };
        let key = CredentialKey::new(DriverId::new(WEBHOOK_SECRET_DRIVER), account);
        let secret = match self.secrets.get(&key) {
            Ok(s) => s,
            Err(_) => return false,
        };
        // Compute the expected tag over the body; compare CONSTANT-TIME (RFD §10).
        let tag = qfs_crypto_core::hmac_sha256(secret.expose(), body);
        let expected_hex = qfs_crypto_core::hex_lower(&tag);
        qfs_crypto_core::constant_time_eq(expected_hex.as_bytes(), provided_hex.as_bytes())
    }
}

/// The webhook ingestion binding: a thin [`qfs_server::Binding`] (kind `Ingest`) over a shared
/// [`WebhookIngest`] core. `reconcile` (sync) converges the route set via the core; the binary
/// holds a clone of the same `Arc<WebhookIngest>` for the HTTP fallback closure, so the listener
/// and the runtime-owned binding share ONE route set + bus.
pub struct WebhookBinding {
    ingest: Arc<WebhookIngest>,
}

impl WebhookBinding {
    /// Construct a binding over a shared secrets surface + event bus (empty route set).
    #[must_use]
    pub fn new(secrets: Arc<dyn Secrets>, bus: Arc<dyn EventBus>) -> Self {
        Self {
            ingest: Arc::new(WebhookIngest::new(secrets, bus)),
        }
    }

    /// The shared ingest core (so the binary's HTTP fallback closure can call `ingest`).
    #[must_use]
    pub fn ingest_core(&self) -> Arc<WebhookIngest> {
        Arc::clone(&self.ingest)
    }

    /// A shared handle to the live route set.
    #[must_use]
    pub fn routes_handle(&self) -> Arc<RwLock<Arc<WebhookRoutes>>> {
        self.ingest.routes_handle()
    }

    /// Snapshot the current live route set.
    #[must_use]
    pub fn current_routes(&self) -> Arc<WebhookRoutes> {
        self.ingest.current_routes()
    }

    /// Ingest a request via the shared core (a convenience delegate for tests).
    pub fn ingest(
        &self,
        route: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        now: i64,
    ) -> IngestOutcome {
        self.ingest.ingest(route, headers, body, now)
    }
}

impl Binding for WebhookBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Ingest
    }

    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError> {
        self.ingest.reconcile_routes(state)
    }
}

/// Helper to build the `v0=<hex>` signature header value for a body under a secret (the producer
/// side — used by tests + a client SDK). Exposed so a fixture can sign a request without
/// re-deriving the scheme.
#[must_use]
pub fn sign_body(secret: &[u8], body: &[u8]) -> String {
    let tag = qfs_crypto_core::hmac_sha256(secret, body);
    format!("{SIG_VERSION}={}", qfs_crypto_core::hex_lower(&tag))
}
