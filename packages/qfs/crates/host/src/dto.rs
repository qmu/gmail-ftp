//! The owned, vendor-free, **wasm-clean** DTOs that cross the [`crate::RuntimeHost`] seam
//! (t36, RFD-0001 §8/§9).
//!
//! These types are produced by the t30 server registry (a `ServerState` snapshot) and consumed
//! **identically** by both deployment hosts — the EC2 daemon and the Cloudflare Worker. No
//! `worker::*`, `tokio::*`, or vendor storage type ever appears here: a `JobBinding` is the same
//! owned data whether a `tokio::time` interval or a CF Cron Trigger fires it. Keeping the seam
//! data pure is what lets the single effect-plan interpreter run unchanged on two disjoint async
//! substrates (the t36 hard part).
//!
//! Every DTO references credentials / policies **by handle / name only** (RFD §10) — a binding
//! carries the `/server/policies` row name, the watcher's source path, the native store's mount
//! name. The generated `wrangler.toml` (see [`crate::wrangler`]) enumerates these names and
//! **never** embeds a token.

use serde::{Deserialize, Serialize};

/// An epoch-second timestamp — the project's standard time instant (matching
/// `qfs_core::Value::Timestamp` / `JobDef.last_run`). `now()` comes from the host because wasm
/// has no `SystemClock` (RFD §6 wasm gotcha); the daemon reads the system clock, the Worker reads
/// the request/event time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Construct from an epoch second.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs)
    }

    /// The raw epoch second.
    #[must_use]
    pub const fn as_secs(self) -> i64 {
        self.0
    }
}

/// An owned key into a [`crate::DurableStore`] (watcher cursors / `LAST_RUN`). A vendor-free
/// string — the daemon maps it to a file path under its state dir, the Worker maps it to a
/// Durable Object storage key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateKey(pub String);

impl StateKey {
    /// Construct from owned text.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The raw key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Owned, opaque durable-state bytes. Watcher cursors and the `LAST_RUN` high-water mark are
/// small (an epoch second, an ETag, a continuation token); the store treats them as bytes so the
/// same `get/put/cas` contract works for a DO storage value or an fsync'd file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateBytes(pub Vec<u8>);

impl StateBytes {
    /// Construct from owned bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for StateBytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

/// Which CF native-storage primitive a `/d1`·`/r2`·`/kv` mount maps to (RFD §8). On the daemon
/// each maps to the driver's existing HTTP client; on the Worker each maps to an `env` binding
/// (`env.d1(name)`, `env.bucket(name)`, `env.kv(name)`). The kind is for the wrangler generator
/// and the audit log; the seam treats every native store uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum NativeStoreKind {
    /// Cloudflare D1 (SQLite-over-HTTP) — `/cf/d1/<db>` → `[[d1_databases]]`.
    D1,
    /// Cloudflare R2 (object store) — `/cf/r2/<bucket>` → `[[r2_buckets]]`.
    R2,
    /// Cloudflare KV (key-value) — `/cf/kv/<ns>` → `[[kv_namespaces]]`.
    Kv,
}

impl NativeStoreKind {
    /// A stable label for the audit log / wrangler section name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            NativeStoreKind::D1 => "d1",
            NativeStoreKind::R2 => "r2",
            NativeStoreKind::Kv => "kv",
        }
    }
}

/// An owned mount reference (RFD §8): the `/cf/d1/<db>` / `/cf/r2/<bucket>` / `/cf/kv/<ns>`
/// address a native-store binding backs. The daemon resolves it to the driver's HTTP client; the
/// Worker resolves it to the `env` binding named [`NativeStoreBinding::binding_name`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Mount(pub String);

impl Mount {
    /// Construct from an owned mount path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// The raw mount path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An ENDPOINT binding (`ENDPOINT`→Worker `fetch` / daemon route). Owned projection of the t30
/// `EndpointDef` — method + route + the optional read-only policy handle. Carries no query AST
/// (the fire-path bindings own that); this is the host-agnostic *cause* description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointBinding {
    /// The handler name.
    pub name: String,
    /// The HTTP method (uppercased), empty if unspecified.
    pub method: String,
    /// The route path, e.g. `/recent`.
    pub route: String,
    /// The optional read-only-policy handle (a `/server/policies` row name). Never a token.
    pub policy: Option<String>,
}

/// A JOB binding (`JOB`→CF Cron Trigger / daemon interval). Owned projection of the t30 `JobDef`.
/// The `cron` field is the wrangler `crontab` expression DERIVED from the `EVERY <interval>` cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBinding {
    /// The job name.
    pub name: String,
    /// The raw `EVERY` interval text (e.g. `1h`, `7d`), empty if unspecified.
    pub every: String,
    /// The derived CF Cron-Trigger `crontab` expression (5-field) for this cadence.
    pub cron: String,
    /// The attached `POLICY` handle (the `/server/policies` row the fired plan commits under).
    /// `None` ⇒ fail-closed default-deny at fire time (t35). Never a token.
    pub policy: Option<String>,
}

/// A WEBHOOK binding (`WEBHOOK`/event→CF Queue / daemon bus + `/hooks/...` ingest). Owned
/// projection of the t30 `WebhookDef`. The `secret` is a `qfs-secrets` HANDLE, never an inline
/// token (RFD §10) — empty for an unsigned (test/internal) webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookBinding {
    /// The webhook name.
    pub name: String,
    /// The inbound route, e.g. `/hooks/x`.
    pub route: String,
    /// The signing-secret HANDLE (a `qfs-secrets` account id), empty for unsigned. Never a token.
    pub secret_handle: String,
    /// The derived CF Queue name this webhook's events publish to (`<name>-events`).
    pub queue: String,
}

/// A watcher / event-trigger binding (`TRIGGER ON <event>`→CF Queue consumer + DO-backed cursor /
/// daemon poll task + cursor). Owned projection of the t30 `TriggerDef`. The watcher's durable
/// cursor and `LAST_RUN` live in [`crate::DurableStore`] (a DO on CF, an fsync'd file on the
/// daemon); [`WatcherBinding::cursor_key`] is the stable [`StateKey`] for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherBinding {
    /// The trigger name.
    pub name: String,
    /// The event/source this trigger watches (raw, e.g. `inbox`), empty if unspecified.
    pub on: String,
    /// The attached `POLICY` handle the fired plan commits under. `None` ⇒ default-deny.
    pub policy: Option<String>,
}

impl WatcherBinding {
    /// The stable durable-state key for this watcher's cursor / `LAST_RUN` high-water mark. The
    /// host maps it to a DO storage key (CF) or a file under the state dir (daemon).
    #[must_use]
    pub fn cursor_key(&self) -> StateKey {
        StateKey::new(format!("watcher/{}/cursor", self.name))
    }
}

/// A native-store binding (`/cf/d1`·`/cf/r2`·`/cf/kv`→CF `env` binding / daemon HTTP client).
/// Derived by mount NAME only — the wrangler generator emits the binding name, never a token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NativeStoreBinding {
    /// Which CF primitive this mount maps to.
    pub kind: NativeStoreKind,
    /// The resource NAME (the D1 database / R2 bucket / KV namespace), parsed from the mount path.
    pub resource: String,
    /// The mount address this binding backs (`/cf/d1/<db>` etc).
    pub mount: Mount,
}

impl NativeStoreBinding {
    /// The wrangler `binding` name the Worker references as `env.<binding_name>` — derived as the
    /// uppercased `<KIND>_<RESOURCE>` (e.g. `D1_ANALYTICS`). Deterministic + name-only.
    #[must_use]
    pub fn binding_name(&self) -> String {
        format!(
            "{}_{}",
            self.kind.label().to_ascii_uppercase(),
            sanitize_binding(&self.resource)
        )
    }
}

/// Sanitize a resource name into a wrangler/env binding identifier (uppercase alnum + `_`).
fn sanitize_binding(resource: &str) -> String {
    resource
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The host-agnostic **binding set** derived from a `/server` registry snapshot (t30). Both hosts
/// consume this identical structure: the daemon turns it into routes/intervals/bus-consumers, the
/// Worker turns it into `fetch`/`scheduled`/`queue` handlers + DO storage + `env` bindings. This
/// is the single deployment contract — adding a deployment adds ZERO keywords (RFD §8).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BindingSet {
    /// ENDPOINT causes (→ `fetch` / route).
    pub endpoints: Vec<EndpointBinding>,
    /// JOB causes (→ Cron Trigger / interval).
    pub jobs: Vec<JobBinding>,
    /// WEBHOOK causes (→ Queue / `/hooks/...` ingest).
    pub webhooks: Vec<WebhookBinding>,
    /// Watcher / TRIGGER causes (→ Queue consumer + DO cursor / poll task + cursor).
    pub watchers: Vec<WatcherBinding>,
    /// Native-store bindings (→ `env.d1()/.bucket()/.kv()` / HTTP client), sorted + de-duplicated.
    pub native_stores: Vec<NativeStoreBinding>,
}

impl BindingSet {
    /// An empty binding set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The total number of bound causes across every kind (the safe-to-log summary count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.endpoints.len()
            + self.jobs.len()
            + self.webhooks.len()
            + self.watchers.len()
            + self.native_stores.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A one-line, secret-free summary (counts per kind) — the audit/log projection.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "endpoints={} jobs={} webhooks={} watchers={} native_stores={}",
            self.endpoints.len(),
            self.jobs.len(),
            self.webhooks.len(),
            self.watchers.len(),
            self.native_stores.len(),
        )
    }
}
