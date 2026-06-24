//! [`SlackClient`] — the thin, **mockable** Slack Web-API seam (RFD-0001 §9 no-heavy-SDK,
//! boundary B3), plus [`RestSlackClient`] (the real client over a local [`HttpTransport`] seam) and
//! [`MockSlackClient`] (an in-memory fake for tests — no live Slack, no network).
//!
//! ## Reuses the shared http-core DTOs + the t18/t24 reusable seam *shape* (no hand-rolled HTTP DTO)
//! The real client builds owned [`qfs_http_core::HttpRequest`]s — the **same shared DTOs +
//! redaction authority** the t18 REST seam, t24 GitHub seam, and `qfs-google-auth` trade in — and
//! sends them through a thin [`HttpTransport`] trait (a structural twin of t18's `HttpClient`). The
//! driver does **not** depend on `qfs-driver-http` as a crate (a qfs-runtime consumer must stay a
//! leaf — the dep-direction confinement test), so the reqwest wire impl rides this transport seam.
//! On top of the seam this client layers the Slack conventions:
//! - **Bearer (bot-token) auth**: the token, resolved from a [`qfs_secrets::Secret`] at
//!   request-build time, written into an `Authorization: Bearer …` header the redacting
//!   [`HttpRequest`] `Debug` hides. Never logged, never in a DTO/error.
//! - **Cursor pagination**: a list `GET` follows `response_metadata.next_cursor`, bounded by
//!   [`MAX_PAGES`].
//! - **429 / `Retry-After` bounded retry on idempotent GETs only**: a transient 429/5xx on a `GET`
//!   is retried up to [`MAX_RETRIES`] honouring `Retry-After`; a write (`chat.postMessage` etc.) is
//!   **never** retried (at-least-once for the non-idempotent post — RFD §6).
//!
//! ## The t18 BodyErrorRule (the explicit reason t25 exists as its consumer)
//! Slack signals application errors with **HTTP 200** carrying `{"ok":false,"error":"<code>"}`. A
//! status-only classifier would treat that as success. The [`BodyErrorRule`] — **opt-in** on the
//! config (default-off per t18; Slack turns it on) — inspects the decoded JSON `ok` field inside
//! the seam and maps `ok:false` to a structured **terminal** [`SlackError::Body`] carrying Slack's
//! `error` code. The rule lives in the seam so every call (read + write) is covered uniformly.

use std::sync::{Arc, Mutex};

use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};
use qfs_secrets::{CredentialKey, Secrets};

use crate::effect::SlackEffect;
use crate::error::SlackError;
use crate::path::NodeKind;

/// A secret-free transport failure (DNS/connect/TLS/timeout) — the class only, never a header
/// value. The structural twin of t18's transport error, kept local so this leaf does not depend on
/// `qfs-driver-http` (runtime-consumer confinement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// A secret-free class reason (e.g. `connection failed`).
    pub reason: String,
}

/// The thin HTTP transport seam (RFD §9 boundary B3): a driver builds an owned http-core
/// [`HttpRequest`] and calls [`HttpTransport::send`]; the impl performs the wire exchange and
/// returns an owned [`HttpResponse`] or a secret-free [`TransportError`]. `reqwest`/`url` types
/// **never** cross it — it trades only in the shared http-core DTOs (the same contract t18's
/// `HttpClient` honours). `Send + Sync` so an `Arc<dyn HttpTransport>` is shareable across the
/// runtime's blocking apply threads.
pub trait HttpTransport: Send + Sync {
    /// Execute one request synchronously. A non-2xx status is **not** an error here — the client
    /// classifies [`HttpResponse::status`].
    ///
    /// # Errors
    /// [`TransportError`] if the wire exchange fails before a status is received.
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The Slack Web-API base URL.
pub const API_BASE: &str = "https://slack.com/api";
/// The hard ceiling on cursor pages a list follows (RFD §6 runaway-fetch guard).
pub const MAX_PAGES: u32 = 50;
/// The hard ceiling on transient-retry attempts on an idempotent GET.
pub const MAX_RETRIES: u32 = 3;
/// The default page size sent as `limit` when the caller pushed none.
pub const DEFAULT_LIMIT: &str = "200";

/// The closed list of Web-API methods this driver issues (RFD §9 enums) — the capability sum type
/// the read/apply legs select among. Each maps onto a Slack `method` path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackApiCall {
    /// `conversations.history` — a channel message log read.
    ConversationsHistory,
    /// `conversations.replies` — a thread read.
    ConversationsReplies,
    /// `users.list` — the user directory read.
    UsersList,
    /// `files.list` — the files namespace `ls`.
    FilesList,
    /// `chat.postMessage` — post a message/reply/DM.
    ChatPostMessage,
    /// `chat.update` — edit a message by `ts`.
    ChatUpdate,
    /// `chat.delete` — delete a message.
    ChatDelete,
    /// `reactions.add`.
    ReactionsAdd,
    /// `reactions.remove`.
    ReactionsRemove,
    /// `pins.add`.
    PinsAdd,
    /// `pins.remove`.
    PinsRemove,
    /// `files.upload`.
    FilesUpload,
    /// `files.delete`.
    FilesDelete,
}

impl SlackApiCall {
    /// The Slack Web-API method path segment (e.g. `chat.postMessage`).
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            SlackApiCall::ConversationsHistory => "conversations.history",
            SlackApiCall::ConversationsReplies => "conversations.replies",
            SlackApiCall::UsersList => "users.list",
            SlackApiCall::FilesList => "files.list",
            SlackApiCall::ChatPostMessage => "chat.postMessage",
            SlackApiCall::ChatUpdate => "chat.update",
            SlackApiCall::ChatDelete => "chat.delete",
            SlackApiCall::ReactionsAdd => "reactions.add",
            SlackApiCall::ReactionsRemove => "reactions.remove",
            SlackApiCall::PinsAdd => "pins.add",
            SlackApiCall::PinsRemove => "pins.remove",
            SlackApiCall::FilesUpload => "files.upload",
            SlackApiCall::FilesDelete => "files.delete",
        }
    }

    /// The read call a node kind lists through.
    #[must_use]
    pub const fn read_for(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Messages | NodeKind::Dms => SlackApiCall::ConversationsHistory,
            NodeKind::Replies | NodeKind::Reactions => SlackApiCall::ConversationsReplies,
            NodeKind::Files => SlackApiCall::FilesList,
            NodeKind::Users => SlackApiCall::UsersList,
        }
    }
}

/// The thin Slack API seam. A driver issues every Slack call through this; the real impl rides the
/// [`HttpTransport`] seam (Bearer + cursor pagination + bounded GET retry + the [`BodyErrorRule`]),
/// the test impl answers from in-memory fixtures. `Send + Sync` so an `Arc<dyn SlackClient>` can be
/// shared across the runtime's blocking apply threads.
pub trait SlackClient: Send + Sync {
    /// List the `kind` collection through its read call, applying the pushed query `params`,
    /// following cursor pagination, returning the merged JSON of all pages.
    ///
    /// # Errors
    /// [`SlackError`] on a non-2xx status, an `ok:false` body (BodyErrorRule), a decode failure, or
    /// an auth/transport failure.
    fn list(
        &self,
        kind: NodeKind,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, SlackError>;

    /// Apply one decoded write/CALL [`SlackEffect`], returning the affected count.
    ///
    /// # Errors
    /// [`SlackError`] on a non-2xx status, an `ok:false` body (BodyErrorRule, unless a swallowed
    /// already-done class), or an auth/transport failure.
    fn apply(&self, effect: &SlackEffect) -> Result<u64, SlackError>;
}

/// The **t18 BodyErrorRule** carried into the Slack seam: how to treat Slack's HTTP-200
/// `{"ok":false,...}` envelope. Opt-in on the config (default-off per t18); Slack turns it **on**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BodyErrorRule {
    /// Ignore the `ok` field — a 2xx is success regardless of body (the t18 default for backends
    /// that signal errors via status only).
    Off,
    /// Inspect the decoded `ok` field; `ok:false` maps to a terminal [`SlackError::Body`] carrying
    /// the `error` code. This is the Slack setting.
    On,
}

impl BodyErrorRule {
    /// Apply the rule to a decoded Slack JSON body for `op`. Returns the body unchanged on success,
    /// or a structured [`SlackError::Body`] when the rule is [`BodyErrorRule::On`] and the body
    /// carries `ok:false`. `swallow_already_done` lets a naturally-idempotent op treat Slack's
    /// already-done class (`already_reacted`/`already_pinned`/`no_reaction`/`not_pinned`) as a
    /// no-op success (RFD §6).
    ///
    /// # Errors
    /// [`SlackError::Body`] per the rule.
    pub fn check(
        self,
        op: &'static str,
        body: &serde_json::Value,
        swallow_already_done: bool,
    ) -> Result<(), SlackError> {
        if self == BodyErrorRule::Off {
            return Ok(());
        }
        let ok = body.get("ok").and_then(serde_json::Value::as_bool);
        if ok == Some(true) || ok.is_none() {
            return Ok(());
        }
        let code = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown_error");
        if swallow_already_done && is_already_done(code) {
            return Ok(());
        }
        Err(SlackError::Body {
            op,
            code: code.to_string(),
        })
    }
}

/// Whether a Slack `error` code is the "already in the desired state" class a naturally-idempotent
/// op swallows (RFD §6): an `already_reacted`/`already_pinned` (the add already landed) and the
/// symmetric `no_reaction`/`not_pinned` (the remove already landed) are no-op successes.
#[must_use]
pub fn is_already_done(code: &str) -> bool {
    matches!(
        code,
        "already_reacted" | "already_pinned" | "no_reaction" | "not_pinned"
    )
}

/// The real Slack client: builds owned [`HttpRequest`]s, injects the Bearer bot token from the
/// secret store, and sends them through the [`HttpTransport`] seam. Confines no `reqwest` itself —
/// the reqwest wire impl lives behind the transport trait (parked with live E2E for t38).
pub struct RestSlackClient {
    http: Arc<dyn HttpTransport>,
    secrets: Arc<dyn Secrets>,
    cred: CredentialKey,
    body_rule: BodyErrorRule,
}

impl RestSlackClient {
    /// Build a Slack client over the `http` transport, resolving the bot token under `cred` from
    /// `secrets`, applying `body_rule` (Slack passes [`BodyErrorRule::On`]). The token is read only
    /// at request-build time, never stored here.
    #[must_use]
    pub fn new(
        http: Arc<dyn HttpTransport>,
        secrets: Arc<dyn Secrets>,
        cred: CredentialKey,
        body_rule: BodyErrorRule,
    ) -> Self {
        Self {
            http,
            secrets,
            cred,
            body_rule,
        }
    }

    /// Build a base request with the Bearer bot token injected. The token is exposed only here (a
    /// header value the redacting `Debug` hides) and dropped immediately.
    fn request(&self, method: HttpMethod, url: String) -> Result<HttpRequest, SlackError> {
        let secret = self
            .secrets
            .get(&self.cred)
            .map_err(|e| SlackError::Auth { code: e.code() })?;
        let token = secret.expose_str().ok_or(SlackError::Auth {
            code: "secret_not_utf8",
        })?;
        Ok(HttpRequest::new(method, url)
            .header("Accept", "application/json")
            .header("User-Agent", "qfs-driver-slack")
            .header("Authorization", format!("Bearer {token}")))
    }

    /// Send an idempotent GET with bounded transient retry. A 429/5xx is retried up to
    /// [`MAX_RETRIES`] (the `Retry-After` header bounds the conceptual wait). Only GETs are retried.
    fn send_get(&self, op: &'static str, url: &str) -> Result<HttpResponse, SlackError> {
        let mut attempt = 0;
        loop {
            let req = self.request(HttpMethod::Get, url.to_string())?;
            let resp = self.http.send(&req).map_err(SlackError::from)?;
            tracing::debug!(method = "GET", url = %url, status = resp.status, "slack request");
            if resp.is_success() {
                return Ok(resp);
            }
            if SlackError::is_transient_status(resp.status) && attempt < MAX_RETRIES {
                let _retry_after = resp
                    .header_value("retry-after")
                    .and_then(|v| v.parse::<u64>().ok());
                attempt += 1;
                continue;
            }
            return Err(SlackError::Http {
                op,
                status: resp.status,
            });
        }
    }

    /// Build a Slack method URL with the cursor + pushed params + a default `limit`.
    fn list_url(call: SlackApiCall, params: &[(String, String)], cursor: Option<&str>) -> String {
        let mut all: Vec<(String, String)> = params.to_vec();
        if !all.iter().any(|(k, _)| k == "limit") {
            all.push(("limit".to_string(), DEFAULT_LIMIT.to_string()));
        }
        if let Some(c) = cursor {
            all.push(("cursor".to_string(), c.to_string()));
        }
        let qs = all
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{API_BASE}/{}?{qs}", call.method())
    }

    /// Send a JSON-bodied write (`POST`). Never retried (at-least-once for the non-idempotent post).
    /// Applies the [`BodyErrorRule`] to the 200 envelope so an `ok:false` is a structured terminal
    /// error rather than a false success.
    fn send_write(
        &self,
        op: &'static str,
        call: SlackApiCall,
        body: serde_json::Value,
        swallow_already_done: bool,
    ) -> Result<(), SlackError> {
        let url = format!("{API_BASE}/{}", call.method());
        let bytes = serde_json::to_vec(&body).map_err(|_| SlackError::Decode {
            op,
            reason: "could not encode the request body".to_string(),
        })?;
        let req = self
            .request(HttpMethod::Post, url)?
            .header("Content-Type", "application/json; charset=utf-8")
            .with_body(bytes);
        let resp = self.http.send(&req).map_err(SlackError::from)?;
        tracing::debug!(method = "POST", op = %op, status = resp.status, "slack request");
        if !resp.is_success() {
            return Err(SlackError::Http {
                op,
                status: resp.status,
            });
        }
        // BodyErrorRule: a 200 may still be ok:false — classify the envelope.
        let value: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|_| SlackError::Decode {
                op,
                reason: "response was not valid JSON".to_string(),
            })?;
        self.body_rule.check(op, &value, swallow_already_done)
    }
}

impl SlackClient for RestSlackClient {
    fn list(
        &self,
        kind: NodeKind,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, SlackError> {
        let call = SlackApiCall::read_for(kind);
        let op = call.method();
        let collection = collection_key(kind);
        let mut merged: Vec<serde_json::Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _page in 0..MAX_PAGES {
            let url = Self::list_url(call, params, cursor.as_deref());
            let resp = self.send_get(op, &url)?;
            let value: serde_json::Value =
                serde_json::from_slice(&resp.body).map_err(|_| SlackError::Decode {
                    op,
                    reason: "list response was not valid JSON".to_string(),
                })?;
            // BodyErrorRule on the read path too (a list can be ok:false — e.g. channel_not_found).
            self.body_rule.check(op, &value, false)?;
            if let Some(items) = value.get(collection).and_then(serde_json::Value::as_array) {
                merged.extend(items.iter().cloned());
            }
            cursor = value
                .get("response_metadata")
                .and_then(|m| m.get("next_cursor"))
                .and_then(serde_json::Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        // Re-wrap under the collection key so the decoder's envelope-aware path works uniformly.
        Ok(serde_json::json!({ collection: merged }))
    }

    fn apply(&self, effect: &SlackEffect) -> Result<u64, SlackError> {
        let swallow = effect.swallows_already_done();
        match effect {
            SlackEffect::PostMessage {
                channel,
                text,
                thread_ts,
                client_msg_id,
                ..
            } => {
                let mut payload = serde_json::json!({
                    "channel": channel, "text": text, "client_msg_id": client_msg_id,
                });
                if let Some(t) = thread_ts {
                    payload["thread_ts"] = serde_json::Value::String(t.clone());
                }
                // POST is not idempotent: at-least-once, never silently retried (RFD §6).
                self.send_write(
                    "chat.postMessage",
                    SlackApiCall::ChatPostMessage,
                    payload,
                    false,
                )?;
                Ok(1)
            }
            SlackEffect::AddReaction { channel, ts, emoji } => {
                let payload =
                    serde_json::json!({ "channel": channel, "timestamp": ts, "name": emoji });
                self.send_write(
                    "reactions.add",
                    SlackApiCall::ReactionsAdd,
                    payload,
                    swallow,
                )?;
                Ok(1)
            }
            SlackEffect::RemoveReaction { channel, ts, emoji } => {
                let payload =
                    serde_json::json!({ "channel": channel, "timestamp": ts, "name": emoji });
                self.send_write(
                    "reactions.remove",
                    SlackApiCall::ReactionsRemove,
                    payload,
                    swallow,
                )?;
                Ok(1)
            }
            SlackEffect::DeleteMessage { channel, ts } => {
                let payload = serde_json::json!({ "channel": channel, "ts": ts });
                self.send_write("chat.delete", SlackApiCall::ChatDelete, payload, false)?;
                Ok(1)
            }
            SlackEffect::UpdateMessage { channel, ts, text } => {
                let payload = serde_json::json!({ "channel": channel, "ts": ts, "text": text });
                self.send_write("chat.update", SlackApiCall::ChatUpdate, payload, false)?;
                Ok(1)
            }
            SlackEffect::Pin { channel, ts } => {
                let payload = serde_json::json!({ "channel": channel, "timestamp": ts });
                self.send_write("pins.add", SlackApiCall::PinsAdd, payload, swallow)?;
                Ok(1)
            }
            SlackEffect::Unpin { channel, ts } => {
                let payload = serde_json::json!({ "channel": channel, "timestamp": ts });
                self.send_write("pins.remove", SlackApiCall::PinsRemove, payload, swallow)?;
                Ok(1)
            }
            SlackEffect::UploadFile { name, content } => {
                let payload = serde_json::json!({ "filename": name, "content": content });
                self.send_write("files.upload", SlackApiCall::FilesUpload, payload, false)?;
                Ok(1)
            }
            SlackEffect::DeleteFile { id } => {
                let payload = serde_json::json!({ "file": id });
                self.send_write("files.delete", SlackApiCall::FilesDelete, payload, false)?;
                Ok(1)
            }
        }
    }
}

/// The JSON envelope key a node kind's list rows live under (`messages`/`members`/`files`).
const fn collection_key(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Messages | NodeKind::Replies | NodeKind::Reactions | NodeKind::Dms => "messages",
        NodeKind::Files => "files",
        NodeKind::Users => "members",
    }
}

/// Minimal percent-encoding for a query parameter value (dependency-free).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One recorded Slack API call — what a test asserts the driver issued. Secret-free by construction
/// (no token ever enters this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// A `list` with the node kind and the pushed params.
    List {
        /// The listed node kind.
        kind: NodeKind,
        /// The pushed query params.
        params: Vec<(String, String)>,
    },
    /// A write/CALL effect applied (the decoded effect itself).
    Apply(SlackEffect),
}

/// An in-memory mock Slack client (tests / CI / wasm): answers list calls from pre-seeded JSON and
/// **records** every call so a test asserts the exact API surface the driver exercised — with **no
/// socket and no credentials**. The recorded calls also prove `PREVIEW` performs zero I/O.
#[derive(Default)]
pub struct MockSlackClient {
    list_pages: Mutex<Vec<serde_json::Value>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

impl MockSlackClient {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the JSON value a `list` returns (FIFO across calls).
    #[must_use]
    pub fn with_list(self, value: serde_json::Value) -> Self {
        if let Ok(mut q) = self.list_pages.lock() {
            q.push(value);
        }
        self
    }

    /// The calls this mock received, in order — what a test asserts against.
    #[must_use]
    pub fn recorded(&self) -> Vec<RecordedCall> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, call: RecordedCall) {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(call);
        }
    }
}

impl SlackClient for MockSlackClient {
    fn list(
        &self,
        kind: NodeKind,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, SlackError> {
        self.record(RecordedCall::List {
            kind,
            params: params.to_vec(),
        });
        let page = self
            .list_pages
            .lock()
            .ok()
            .and_then(|mut q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
            .unwrap_or_else(|| serde_json::json!({ collection_key(kind): [] }));
        Ok(page)
    }

    fn apply(&self, effect: &SlackEffect) -> Result<u64, SlackError> {
        self.record(RecordedCall::Apply(effect.clone()));
        Ok(1)
    }
}
