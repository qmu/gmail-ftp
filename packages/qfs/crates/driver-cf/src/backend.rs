//! [`CfBackend`] — the thin, **mockable** Cloudflare transport seam (RFD-0001 §9 no-heavy-SDK,
//! boundary B3), plus the owned DTOs it trades in, the real [`HttpApiBackend`] (REST over a local
//! [`HttpExchange`] seam — the `qfs-google-auth` precedent, so this crate does not depend on
//! `qfs-driver-http` and stays an independent runtime leaf), and [`MockCfBackend`] (in-memory
//! fixtures for tests — no live Cloudflare, no network).
//!
//! The trait trades **only** in owned, vendor-free DTOs; Cloudflare JSON and `worker::*` env
//! bindings never cross it. The D1 leg ships the **already-rendered** `(sql, params)` from the
//! reused t17 sqlite emitter with `params` as a **structured bound array** — never interpolated
//! into the SQL — the headline injection-safety obligation an HTTP backend carries (the t17
//! Architect flagged this). The D1 `/batch` endpoint maps one [`CfBackend::d1_batch`] to one
//! atomic transaction.
//!
//! ## Token discipline (RFD §10)
//! The Cloudflare API token is a [`qfs_secrets::Secret`] resolved at construction and written
//! into the `Authorization: Bearer` header. It is **never** logged (the [`HttpRequest`] `Debug`
//! redacts the header via the shared `qfs-http-core` authority), never stored in a DTO, never in
//! a [`CfError`].
//!
//! ## Dual transport (named park)
//! The wasm `WorkersBindingBackend` (native `worker` env bindings) is **parked**: there is no
//! live wasm CI lane yet, and the DTOs + this seam are wasm-clean so the binding impl drops in
//! later behind the identical trait, producing identical DTOs (the ticket's conformance goal).

use std::sync::{Arc, Mutex};

use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};
use qfs_secrets::Secret;
use qfs_sql_core::Param;
use qfs_types::{Row, Value};

use crate::error::CfError;

/// A transport failure before an HTTP status was received — secret-free (built from the request
/// shape only). The error half of the local [`HttpExchange`] seam (mirrors the t19 auth base's
/// `TransportError`); kept local so this crate does NOT depend on `qfs-driver-http` (a runtime
/// leaf) and stays an independent runtime leaf.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport error for {method} {url}: {reason}")]
pub struct TransportError {
    /// The HTTP method (uppercase token).
    pub method: String,
    /// The request URL (secret-free).
    pub url: String,
    /// A secret-free reason (the transport's class, never a header value).
    pub reason: String,
}

/// The thin **synchronous** transport seam the [`HttpApiBackend`] sends owned [`HttpRequest`]s
/// over (the `qfs-google-auth` `HttpExchange` precedent). A non-2xx status is **not** an error —
/// it rides in the [`HttpResponse`] so the backend classifies it (e.g. a 404 KV miss). The
/// production binary adapts an `Arc<dyn qfs_driver_http::HttpClient>` to this with a trivial DTO
/// copy; `reqwest` stays confined in `qfs-driver-http` and never crosses this boundary (RFD §9).
///
/// `Send + Sync` so an `Arc<dyn HttpExchange>` can be shared across the runtime bridge's blocking
/// apply threads.
pub trait HttpExchange: Send + Sync {
    /// Execute one request synchronously.
    ///
    /// # Errors
    /// [`TransportError`] if the wire exchange fails before a status is received.
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl HttpExchange for Arc<dyn HttpExchange> {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).exchange(req)
    }
}

/// An in-memory mock transport (tests / CI / wasm): records every request and answers from a FIFO
/// queue of scripted responses — so a test asserts the exact request shape the backend built
/// (method, URL, headers, body) **without any socket**. Mirrors the t19 `MockExchange`.
#[derive(Default)]
pub struct MockExchange {
    responses: Mutex<std::collections::VecDeque<Result<HttpResponse, TransportError>>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl MockExchange {
    /// An empty mock (every `exchange` after the queue drains returns a terminal transport error).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one scripted success response (consumed FIFO).
    #[must_use]
    pub fn with_response(self, resp: HttpResponse) -> Self {
        if let Ok(mut q) = self.responses.lock() {
            q.push_back(Ok(resp));
        }
        self
    }

    /// The requests this mock received, in order — what a test asserts against.
    #[must_use]
    pub fn recorded(&self) -> Vec<HttpRequest> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl HttpExchange for MockExchange {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(req.clone());
        }
        let next = self.responses.lock().ok().and_then(|mut q| q.pop_front());
        next.unwrap_or_else(|| {
            Err(TransportError {
                method: req.method.as_str().to_string(),
                url: req.url.clone(),
                reason: "mock exhausted: no scripted response".to_string(),
            })
        })
    }
}

/// One KV entry — the owned DTO a `kv_get`/`kv_list` yields and a `kv_put` carries. Owned,
/// vendor-free. `value` is the stored bytes; `metadata` is the small JSON-ish blob Cloudflare KV
/// stores alongside a value; `expiration_ttl` is the optional TTL (seconds) a put requests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct KvEntry {
    /// The entry key.
    pub key: String,
    /// The stored value bytes (the body of a `kv_get` / the body to `kv_put`).
    pub value: Vec<u8>,
    /// The optional metadata string Cloudflare KV stores next to the value (JSON text).
    pub metadata: Option<String>,
    /// The optional TTL (seconds) a `kv_put` requests; `None` for a non-expiring write.
    pub expiration_ttl: Option<u64>,
}

impl KvEntry {
    /// Construct a KV entry with just a key + value (no metadata/TTL).
    #[must_use]
    pub fn new(key: impl Into<String>, value: Vec<u8>) -> Self {
        Self {
            key: key.into(),
            value,
            metadata: None,
            expiration_ttl: None,
        }
    }

    /// Builder: attach a metadata string.
    #[must_use]
    pub fn with_metadata(mut self, metadata: impl Into<String>) -> Self {
        self.metadata = Some(metadata.into());
        self
    }

    /// Builder: attach a TTL (seconds).
    #[must_use]
    pub fn with_ttl(mut self, ttl: u64) -> Self {
        self.expiration_ttl = Some(ttl);
        self
    }

    /// Project this entry onto the degenerate `(key, value)` table row (the KV-as-table view).
    /// `value` is surfaced as `Text` when it is valid UTF-8, else `Bytes`.
    #[must_use]
    pub fn to_kv_row(&self) -> Row {
        let value = match String::from_utf8(self.value.clone()) {
            Ok(text) => Value::Text(text),
            Err(_) => Value::Bytes(self.value.clone()),
        };
        Row::new(vec![Value::Text(self.key.clone()), value])
    }
}

/// A queue message id — the owned handle a `queue_send` returns (the Cloudflare-assigned id).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MsgId(pub String);

impl MsgId {
    /// Construct a message id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One pulled queue message — the owned DTO a `queue_pull` (tail) yields. Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueueMsg {
    /// The Cloudflare-assigned message id.
    pub id: String,
    /// The message body bytes.
    pub body: Vec<u8>,
    /// How many delivery attempts this message has had (at-least-once delivery — RFD §6).
    pub attempts: u32,
}

impl QueueMsg {
    /// Construct a queue message.
    #[must_use]
    pub fn new(id: impl Into<String>, body: Vec<u8>, attempts: u32) -> Self {
        Self {
            id: id.into(),
            body,
            attempts,
        }
    }

    /// Project this message onto the queue tail row `(id, body, attempts)`.
    #[must_use]
    pub fn to_queue_row(&self) -> Row {
        let body = match String::from_utf8(self.body.clone()) {
            Ok(text) => Value::Text(text),
            Err(_) => Value::Bytes(self.body.clone()),
        };
        Row::new(vec![
            Value::Text(self.id.clone()),
            body,
            Value::Int(i64::from(self.attempts)),
        ])
    }
}

/// The Cloudflare transport seam every backend implements (RFD §9). The driver's D1 SQL
/// compile/emit (reused from t17), the KV blob verbs, and the queue append/tail are written once
/// against this trait; the REST [`HttpApiBackend`] and the parked wasm binding backend are
/// interchangeable impls producing **identical** owned DTOs. No `reqwest`/`worker` type crosses
/// this boundary.
///
/// `Send + Sync` so a backend can be shared across the runtime bridge's blocking apply threads.
pub trait CfBackend: Send + Sync {
    /// Execute a **parameterized** D1 query and return owned rows. `sql` is the t17
    /// sqlite-rendered statement (only `?` placeholders + quoted identifiers); `params` is the
    /// **structured bound array** the backend binds positionally — it is **never** interpolated
    /// into `sql` (the injection-safety invariant). Used for SELECT.
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status, a decode failure, or a transport failure.
    fn d1_query(&self, db: &str, sql: &str, params: &[Param]) -> Result<Vec<Row>, CfError>;

    /// Apply a batch of parameterized D1 statements inside **one atomic transaction** via the
    /// Cloudflare D1 `/batch` endpoint (the D1 ACID story — D1 has no interactive BEGIN/COMMIT,
    /// so a batch IS the transaction). Returns the total affected row count.
    ///
    /// # Errors
    /// [`CfError`] (the batch rolled back) on any statement failure.
    fn d1_batch(&self, db: &str, statements: &[(String, Vec<Param>)]) -> Result<u64, CfError>;

    /// Fetch a single KV entry by key (`None` if absent).
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx (other than 404) status or a transport failure.
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<KvEntry>, CfError>;

    /// Create-or-replace a KV entry (the retry-safe `UPSERT`/`cp` write).
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status or a transport failure.
    fn kv_put(&self, ns: &str, entry: &KvEntry) -> Result<(), CfError>;

    /// Delete a KV entry by key (idempotent — deleting an absent key is success).
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status or a transport failure.
    fn kv_delete(&self, ns: &str, key: &str) -> Result<(), CfError>;

    /// List keys in a namespace, optionally filtered by `prefix`, capped at `limit`.
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status or a transport failure.
    fn kv_list(
        &self,
        ns: &str,
        prefix: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<String>, CfError>;

    /// Send (append) one message to a queue. `idempotency_key` makes at-least-once retries
    /// safe — Cloudflare de-dupes a resend carrying the same key, so no double-append (RFD §6).
    /// Returns the assigned message id.
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status or a transport failure.
    fn queue_send(&self, queue: &str, body: &[u8], idempotency_key: &str)
        -> Result<MsgId, CfError>;

    /// Pull (tail) up to `max` recent messages from a queue (consumer pull). Bounded-tail only —
    /// Queues is not random-access, so there is no WHERE/offset (capabilities advertise exactly
    /// that).
    ///
    /// # Errors
    /// [`CfError`] on a non-2xx status or a transport failure.
    fn queue_pull(&self, queue: &str, max: u32) -> Result<Vec<QueueMsg>, CfError>;
}

/// The Cloudflare REST API base. Real account routing (`/accounts/<id>/...`) is config the
/// caller injects via [`HttpApiBackend::new`]; the op paths below are appended to it.
const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// The real Cloudflare REST backend: builds owned [`HttpRequest`]s and sends them over the local
/// [`HttpExchange`] seam (the production binary adapts a confined `qfs_driver_http::HttpClient` to
/// it; reqwest stays inside that crate). The API token is a [`qfs_secrets::Secret`] written into
/// the `Authorization: Bearer` header at request-build time and **never** logged.
pub struct HttpApiBackend {
    exchange: Arc<dyn HttpExchange>,
    account_id: String,
    token: Secret,
}

impl HttpApiBackend {
    /// Build a REST backend over `exchange`, routing to Cloudflare account `account_id`, bearing
    /// the resolved API `token` (a [`Secret`]). The token is held only to inject the bearer
    /// header; it is never rendered.
    #[must_use]
    pub fn new(
        exchange: Arc<dyn HttpExchange>,
        account_id: impl Into<String>,
        token: Secret,
    ) -> Self {
        Self {
            exchange,
            account_id: account_id.into(),
            token,
        }
    }

    /// Build a request with the bearer + content-type headers attached. The token is exposed
    /// **only** here, at request-build time, into a header the redacting `HttpRequest` `Debug`
    /// hides — it never reaches a log line or a `CfError`.
    fn authed(&self, method: HttpMethod, url: String) -> HttpRequest {
        let bearer = format!("Bearer {}", self.token.expose_str().unwrap_or_default());
        HttpRequest::new(method, url)
            .header("Authorization", bearer)
            .header("Content-Type", "application/json")
    }

    /// Send a request, mapping a transport failure to a secret-free [`CfError::Transport`] and a
    /// non-2xx status to [`CfError::Api`] under `op`.
    fn send(&self, op: &'static str, req: &HttpRequest) -> Result<HttpResponse, CfError> {
        let resp = self
            .exchange
            .exchange(req)
            .map_err(|_| CfError::Transport {
                reason: "cloudflare request could not be completed".to_string(),
            })?;
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(CfError::Api {
                op,
                status: resp.status,
            })
        }
    }

    /// Parse a response body as JSON, mapping a failure to a secret-free decode error.
    fn parse_json(op: &'static str, resp: &HttpResponse) -> Result<serde_json::Value, CfError> {
        serde_json::from_slice(&resp.body).map_err(|_| CfError::Decode {
            op,
            reason: "response body was not valid JSON".to_string(),
        })
    }

    /// The D1 database REST path for a `(db)` under the configured account.
    fn d1_path(&self, db: &str, suffix: &str) -> String {
        format!(
            "{API_BASE}/accounts/{}/d1/database/{db}{suffix}",
            self.account_id
        )
    }

    /// The KV namespace REST path.
    fn kv_path(&self, ns: &str, suffix: &str) -> String {
        format!(
            "{API_BASE}/accounts/{}/storage/kv/namespaces/{ns}{suffix}",
            self.account_id
        )
    }

    /// The Queues REST path.
    fn queue_path(&self, queue: &str, suffix: &str) -> String {
        format!(
            "{API_BASE}/accounts/{}/queues/{queue}{suffix}",
            self.account_id
        )
    }
}

/// Render a bound [`Param`] into the JSON scalar the D1 REST `params` array carries. This is the
/// **structured bound array** form — the value rides in JSON `params`, NEVER in the SQL text, so
/// a `'; DROP TABLE t; --` literal is inert data (the injection-safety invariant).
#[must_use]
pub fn param_to_json(param: &Param) -> serde_json::Value {
    match param {
        Param::Null => serde_json::Value::Null,
        Param::Bool(b) => serde_json::Value::Bool(*b),
        Param::Int(n) => serde_json::Value::from(*n),
        Param::Float(f) => serde_json::Value::from(*f),
        Param::Text(t) => serde_json::Value::String(t.clone()),
        // D1 binds bytes as base64-ish text via the JSON array; we surface the lossy UTF-8 view
        // for a text-y blob, else a JSON array of byte ints — still a single bound array element,
        // never interpolated.
        Param::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => serde_json::Value::String(s.to_string()),
            Err(_) => serde_json::Value::Array(
                b.iter()
                    .map(|byte| serde_json::Value::from(*byte))
                    .collect(),
            ),
        },
    }
}

/// Decode the D1 `result[0].results` rows (an array of JSON objects) into owned [`Row`]s. The
/// column order follows the first object's key order (D1 returns named columns); a `null`
/// becomes [`Value::Null`]. Vendor-free — no D1 type crosses.
fn decode_d1_rows(json: &serde_json::Value) -> Vec<Row> {
    let Some(results) = json
        .get("result")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("results"))
        .and_then(|r| r.as_array())
    else {
        return Vec::new();
    };
    results.iter().filter_map(json_object_to_row).collect()
}

/// Project one D1 JSON object into a [`Row`] in key order.
fn json_object_to_row(obj: &serde_json::Value) -> Option<Row> {
    let map = obj.as_object()?;
    Some(Row::new(map.values().map(json_to_value).collect()))
}

/// Translate a D1 JSON scalar into the owned [`Value`].
fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or(Value::Null),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

/// Read the total affected count out of a D1 batch/query response (`result[].meta.changes`),
/// summed across statements.
fn decode_d1_affected(json: &serde_json::Value) -> u64 {
    json.get("result")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|stmt| {
                    stmt.get("meta")
                        .and_then(|m| m.get("changes"))
                        .and_then(serde_json::Value::as_u64)
                })
                .sum()
        })
        .unwrap_or(0)
}

impl CfBackend for HttpApiBackend {
    fn d1_query(&self, db: &str, sql: &str, params: &[Param]) -> Result<Vec<Row>, CfError> {
        let op = "d1.query";
        // The bound values ride in `params` as a STRUCTURED JSON ARRAY; `sql` carries only `?`
        // placeholders + quoted identifiers (rendered by the reused t17 sqlite emitter). No value
        // is ever interpolated into `sql` — the injection-safety invariant.
        let body = serde_json::json!({
            "sql": sql,
            "params": params.iter().map(param_to_json).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&body).map_err(|_| CfError::Decode {
            op,
            reason: "could not encode the d1 query body".to_string(),
        })?;
        let req = self
            .authed(HttpMethod::Post, self.d1_path(db, "/query"))
            .with_body(bytes);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        Ok(decode_d1_rows(&json))
    }

    fn d1_batch(&self, db: &str, statements: &[(String, Vec<Param>)]) -> Result<u64, CfError> {
        let op = "d1.batch";
        // One /batch request = one atomic transaction (D1 has no interactive BEGIN/COMMIT). Each
        // statement carries its own bound `params` array — never interpolated.
        let batch: Vec<serde_json::Value> = statements
            .iter()
            .map(|(sql, params)| {
                serde_json::json!({
                    "sql": sql,
                    "params": params.iter().map(param_to_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        let body = serde_json::json!({ "batch": batch });
        let bytes = serde_json::to_vec(&body).map_err(|_| CfError::Decode {
            op,
            reason: "could not encode the d1 batch body".to_string(),
        })?;
        let req = self
            .authed(HttpMethod::Post, self.d1_path(db, "/batch"))
            .with_body(bytes);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        Ok(decode_d1_affected(&json))
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<KvEntry>, CfError> {
        let op = "kv.get";
        let req = self.authed(HttpMethod::Get, self.kv_path(ns, &format!("/values/{key}")));
        let resp = self
            .exchange
            .exchange(&req)
            .map_err(|_| CfError::Transport {
                reason: "cloudflare kv get could not be completed".to_string(),
            })?;
        if resp.status == 404 {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CfError::Api {
                op,
                status: resp.status,
            });
        }
        Ok(Some(KvEntry::new(key.to_string(), resp.body)))
    }

    fn kv_put(&self, ns: &str, entry: &KvEntry) -> Result<(), CfError> {
        let op = "kv.put";
        let mut url = self.kv_path(ns, &format!("/values/{}", entry.key));
        if let Some(ttl) = entry.expiration_ttl {
            url.push_str(&format!("?expiration_ttl={ttl}"));
        }
        let req = self
            .authed(HttpMethod::Put, url)
            .with_body(entry.value.clone());
        self.send(op, &req)?;
        Ok(())
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<(), CfError> {
        let op = "kv.delete";
        let req = self.authed(
            HttpMethod::Delete,
            self.kv_path(ns, &format!("/values/{key}")),
        );
        let resp = self
            .exchange
            .exchange(&req)
            .map_err(|_| CfError::Transport {
                reason: "cloudflare kv delete could not be completed".to_string(),
            })?;
        // A delete of an absent key is success (idempotent).
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(CfError::Api {
                op,
                status: resp.status,
            })
        }
    }

    fn kv_list(
        &self,
        ns: &str,
        prefix: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<String>, CfError> {
        let op = "kv.list";
        let mut url = self.kv_path(ns, "/keys");
        let mut query: Vec<String> = Vec::new();
        if let Some(p) = prefix {
            query.push(format!("prefix={p}"));
        }
        if let Some(n) = limit {
            query.push(format!("limit={n}"));
        }
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        let req = self.authed(HttpMethod::Get, url);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        Ok(json
            .get("result")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|k| k.get("name").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn queue_send(
        &self,
        queue: &str,
        body: &[u8],
        idempotency_key: &str,
    ) -> Result<MsgId, CfError> {
        let op = "queue.send";
        let body_text = String::from_utf8_lossy(body).to_string();
        let payload = serde_json::json!({
            "body": body_text,
            "idempotency_key": idempotency_key,
        });
        let bytes = serde_json::to_vec(&payload).map_err(|_| CfError::Decode {
            op,
            reason: "could not encode the queue send body".to_string(),
        })?;
        let req = self
            .authed(HttpMethod::Post, self.queue_path(queue, "/messages"))
            .with_body(bytes);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        let id = json
            .get("result")
            .and_then(|r| r.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| idempotency_key.to_string());
        Ok(MsgId::new(id))
    }

    fn queue_pull(&self, queue: &str, max: u32) -> Result<Vec<QueueMsg>, CfError> {
        let op = "queue.pull";
        let payload = serde_json::json!({ "batch_size": max });
        let bytes = serde_json::to_vec(&payload).map_err(|_| CfError::Decode {
            op,
            reason: "could not encode the queue pull body".to_string(),
        })?;
        let req = self
            .authed(HttpMethod::Post, self.queue_path(queue, "/messages/pull"))
            .with_body(bytes);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        Ok(json
            .get("result")
            .and_then(|r| r.get("messages"))
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|m| {
                        let id = m
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let body = m
                            .get("body")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .as_bytes()
                            .to_vec();
                        let attempts = m
                            .get("attempts")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(1) as u32;
                        QueueMsg::new(id, body, attempts)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// One recorded Cloudflare backend call (the op + its salient owned arguments) — what a test
/// asserts the driver issued. Secret-free by construction (no token ever enters this seam). The
/// D1 arms carry the rendered `sql` + the structured `params` array so a test asserts both the
/// `?`-only SQL and the bound (never-interpolated) values.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// `d1.query` (SELECT) with the rendered SQL + bound params.
    D1Query {
        /// The D1 database.
        db: String,
        /// The rendered SQL (only `?` placeholders).
        sql: String,
        /// The structured bound params array (the values — never interpolated into `sql`).
        params: Vec<Param>,
    },
    /// `d1.batch` — one atomic transaction of N statements (the batch atomicity proof).
    D1Batch {
        /// The D1 database.
        db: String,
        /// Each statement's rendered SQL + bound params.
        statements: Vec<(String, Vec<Param>)>,
    },
    /// `kv.get`.
    KvGet {
        /// The namespace.
        ns: String,
        /// The key.
        key: String,
    },
    /// `kv.put`.
    KvPut {
        /// The namespace.
        ns: String,
        /// The written entry.
        entry: KvEntry,
    },
    /// `kv.delete`.
    KvDelete {
        /// The namespace.
        ns: String,
        /// The deleted key.
        key: String,
    },
    /// `kv.list`.
    KvList {
        /// The namespace.
        ns: String,
        /// The optional key prefix.
        prefix: Option<String>,
        /// The optional cap.
        limit: Option<u32>,
    },
    /// `queue.send` (carries the idempotency key — the at-least-once de-dupe proof).
    QueueSend {
        /// The queue.
        queue: String,
        /// The message body.
        body: Vec<u8>,
        /// The idempotency key.
        idempotency_key: String,
    },
    /// `queue.pull` (tail, capped at `max`).
    QueuePull {
        /// The queue.
        queue: String,
        /// The tail cap.
        max: u32,
    },
}

/// An in-memory mock Cloudflare backend (tests / CI / wasm): answers from pre-seeded fixtures and
/// **records** every call so a test asserts the exact API surface the driver exercised — with
/// **no socket and no credentials**. The recorded D1 calls carry the rendered SQL + the bound
/// params so a test proves the params are a structured array and never interpolated into the SQL.
#[derive(Default)]
pub struct MockCfBackend {
    d1_rows: Mutex<Vec<Vec<Row>>>,
    d1_affected: Mutex<u64>,
    kv_entries: Mutex<Vec<KvEntry>>,
    kv_keys: Mutex<Vec<String>>,
    queue_msgs: Mutex<Vec<QueueMsg>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

impl MockCfBackend {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one D1 SELECT result page (FIFO, consumed by `d1_query`).
    #[must_use]
    pub fn with_d1_rows(self, rows: Vec<Row>) -> Self {
        if let Ok(mut q) = self.d1_rows.lock() {
            q.push(rows);
        }
        self
    }

    /// Seed the affected count a `d1_batch` reports.
    #[must_use]
    pub fn with_d1_affected(self, affected: u64) -> Self {
        if let Ok(mut a) = self.d1_affected.lock() {
            *a = affected;
        }
        self
    }

    /// Seed a KV entry `kv_get` returns (matched by key).
    #[must_use]
    pub fn with_kv_entry(self, entry: KvEntry) -> Self {
        if let Ok(mut e) = self.kv_entries.lock() {
            e.push(entry);
        }
        self
    }

    /// Seed the keys `kv_list` returns.
    #[must_use]
    pub fn with_kv_keys(self, keys: Vec<String>) -> Self {
        if let Ok(mut k) = self.kv_keys.lock() {
            *k = keys;
        }
        self
    }

    /// Seed a queue message `queue_pull` returns.
    #[must_use]
    pub fn with_queue_msg(self, msg: QueueMsg) -> Self {
        if let Ok(mut m) = self.queue_msgs.lock() {
            m.push(msg);
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

impl CfBackend for MockCfBackend {
    fn d1_query(&self, db: &str, sql: &str, params: &[Param]) -> Result<Vec<Row>, CfError> {
        self.record(RecordedCall::D1Query {
            db: db.to_string(),
            sql: sql.to_string(),
            params: params.to_vec(),
        });
        Ok(self
            .d1_rows
            .lock()
            .ok()
            .and_then(|mut q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
            .unwrap_or_default())
    }

    fn d1_batch(&self, db: &str, statements: &[(String, Vec<Param>)]) -> Result<u64, CfError> {
        self.record(RecordedCall::D1Batch {
            db: db.to_string(),
            statements: statements.to_vec(),
        });
        Ok(self.d1_affected.lock().map(|a| *a).unwrap_or(0))
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<KvEntry>, CfError> {
        self.record(RecordedCall::KvGet {
            ns: ns.to_string(),
            key: key.to_string(),
        });
        Ok(self
            .kv_entries
            .lock()
            .ok()
            .and_then(|e| e.iter().find(|x| x.key == key).cloned()))
    }

    fn kv_put(&self, ns: &str, entry: &KvEntry) -> Result<(), CfError> {
        self.record(RecordedCall::KvPut {
            ns: ns.to_string(),
            entry: entry.clone(),
        });
        Ok(())
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<(), CfError> {
        self.record(RecordedCall::KvDelete {
            ns: ns.to_string(),
            key: key.to_string(),
        });
        Ok(())
    }

    fn kv_list(
        &self,
        ns: &str,
        prefix: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<String>, CfError> {
        self.record(RecordedCall::KvList {
            ns: ns.to_string(),
            prefix: prefix.map(str::to_string),
            limit,
        });
        Ok(self.kv_keys.lock().map(|k| k.clone()).unwrap_or_default())
    }

    fn queue_send(
        &self,
        queue: &str,
        body: &[u8],
        idempotency_key: &str,
    ) -> Result<MsgId, CfError> {
        self.record(RecordedCall::QueueSend {
            queue: queue.to_string(),
            body: body.to_vec(),
            idempotency_key: idempotency_key.to_string(),
        });
        Ok(MsgId::new(format!("msg-{idempotency_key}")))
    }

    fn queue_pull(&self, queue: &str, max: u32) -> Result<Vec<QueueMsg>, CfError> {
        self.record(RecordedCall::QueuePull {
            queue: queue.to_string(),
            max,
        });
        let msgs = self
            .queue_msgs
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        Ok(msgs.into_iter().take(max as usize).collect())
    }
}
