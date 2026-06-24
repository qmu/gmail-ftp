//! The [`HttpClient`] seam (RFD-0001 §9 "no heavy vendor SDKs", boundary B3): a **thin**
//! trait the driver sends requests through, plus the concrete [`ReqwestClient`] (the real
//! `reqwest` impl, confined to this crate) and an in-memory [`MockHttpClient`] (tests).
//!
//! The trait is **synchronous** — `send(&self, req) -> Result<HttpResponse, HttpError>` —
//! so the driver's [`qfs_plan::PlanApplier`] apply leg stays synchronous like every other
//! driver (the runtime [`qfs_runtime::PlanApplierBridge`] already offloads it to a tokio
//! blocking thread, so blocking I/O here never stalls a runtime worker, and **no async
//! runtime leaks out of the synchronous applier seam**). `reqwest`/`url` types **never**
//! cross this trait — it trades only in the owned [`HttpRequest`]/[`HttpResponse`] DTOs.

use std::sync::Arc;

use crate::error::HttpError;
use crate::request::{HttpRequest, HttpResponse};

/// The thin HTTP transport seam. A driver builds an owned [`HttpRequest`] and calls
/// [`HttpClient::send`]; the implementation performs the wire exchange and returns an owned
/// [`HttpResponse`] or a structured, secret-free [`HttpError`]. `Send + Sync` so an
/// `Arc<dyn HttpClient>` can be shared across the runtime's blocking apply threads.
pub trait HttpClient: Send + Sync {
    /// Execute one request synchronously.
    ///
    /// # Errors
    /// Returns [`HttpError::Transport`] if the wire exchange fails before a status is
    /// received. A non-2xx status is **not** an error here — the driver classifies the
    /// returned [`HttpResponse::status`] (so a 404 body is still available to decode).
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError>;
}

impl HttpClient for Arc<dyn HttpClient> {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        (**self).send(req)
    }
}

/// The real reqwest client (confined to this crate). Holds the **async** `reqwest::Client`
/// plus a dedicated current-thread `tokio` runtime the synchronous [`HttpClient::send`] drives
/// each request on via `block_on`. The runtime is owned for the client's whole lifetime (never
/// dropped mid-call), and `block_on` runs on the bridge's `spawn_blocking` thread — which has
/// no enclosing runtime entered, so there is no nested-runtime hazard. The per-request timeout
/// surfaces a hung endpoint as a transport error rather than wedging the thread (RFD §6).
pub struct ReqwestClient {
    inner: reqwest::Client,
    /// Owned for the client's whole lifetime. Held in an `Option` so [`Drop`] can take it and
    /// call `shutdown_background()` — a non-blocking shutdown that is safe even when the client
    /// is dropped from within an async context (the `#[tokio::test]` case), avoiding the
    /// "cannot drop a runtime in an async context" panic a blocking drop would cause.
    rt: Option<tokio::runtime::Runtime>,
}

impl Drop for ReqwestClient {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl ReqwestClient {
    /// Build a client with a per-request timeout (seconds). Panic-free (lib policy): if the
    /// HTTP client or the request-driving runtime cannot be built (an environment failure),
    /// the client is constructed with no runtime, and every [`HttpClient::send`] then returns a
    /// structured transport error rather than panicking.
    #[must_use]
    pub fn new(timeout_secs: u64) -> Self {
        let inner = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok();
        Self { inner, rt }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new(30)
    }
}

impl HttpClient for ReqwestClient {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        // Derive the reqwest method from the DTO's canonical uppercase wire token. Going through
        // `as_str()` keeps this total over `qfs_http_core::HttpMethod` even though it is a foreign
        // `#[non_exhaustive]` enum (a future variant just yields its token, no `_ => panic` arm),
        // and the token is always a valid method name so the fallback is unreachable in practice.
        let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let mut builder = self.inner.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }
        let method_token = req.method.as_str();
        let url = req.url.clone();
        // The runtime is always `Some` between construction and Drop.
        let rt = self.rt.as_ref().ok_or_else(|| HttpError::Transport {
            method: method_token.to_string(),
            url: url.clone(),
            reason: "http client runtime unavailable".to_string(),
        })?;
        // Drive the async request to completion on the owned runtime. A transport failure
        // carries only the request shape (method + URL) and the error's class — never a header
        // value (RFD §10).
        rt.block_on(async move {
            let resp = builder.send().await.map_err(|e| HttpError::Transport {
                method: method_token.to_string(),
                url: url.clone(),
                reason: transport_reason(&e),
            })?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>();
            let body = resp
                .bytes()
                .await
                .map_err(|e| HttpError::Transport {
                    method: method_token.to_string(),
                    url: url.clone(),
                    reason: transport_reason(&e),
                })?
                .to_vec();
            let mut out = HttpResponse::new(status, body);
            out.headers = headers;
            Ok(out)
        })
    }
}

/// A secret-free, class-only description of a `reqwest` transport failure. Reports *what kind*
/// of failure (timeout / connect / request) without interpolating a header value.
fn transport_reason(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else if e.is_request() {
        "request could not be sent".to_string()
    } else {
        "transport error".to_string()
    }
}

/// An in-memory mock client (tests / CI / wasm). It records every request it receives and
/// answers from a **queue of scripted responses** (FIFO) — so a test asserts the exact
/// request shape the driver built (method, URL, headers, body) **without any socket**, and
/// drives the multi-page pagination path by queueing several responses.
///
/// No live network, no credentials of its own. The recorded requests are inspected to prove
/// auth headers are injected — and, via the redacting [`HttpRequest`] `Debug`, that they are
/// never logged.
#[derive(Default)]
pub struct MockHttpClient {
    responses: std::sync::Mutex<std::collections::VecDeque<Result<HttpResponse, HttpError>>>,
    recorded: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockHttpClient {
    /// An empty mock (every `send` after the queue drains returns a terminal transport error).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one scripted success response (consumed FIFO by `send`).
    #[must_use]
    pub fn with_response(self, resp: HttpResponse) -> Self {
        self.push(Ok(resp));
        self
    }

    /// Queue one scripted error (e.g. a transport failure) consumed FIFO by `send`.
    #[must_use]
    pub fn with_error(self, err: HttpError) -> Self {
        self.push(Err(err));
        self
    }

    /// Queue a response after construction (the multi-page pagination helper).
    pub fn push_response(&self, resp: HttpResponse) {
        self.push(Ok(resp));
    }

    fn push(&self, item: Result<HttpResponse, HttpError>) {
        if let Ok(mut q) = self.responses.lock() {
            q.push_back(item);
        }
    }

    /// The requests this mock received, in order — what a test asserts against (method, URL,
    /// headers, body). Returns clones so the test holds an owned snapshot.
    #[must_use]
    pub fn recorded(&self) -> Vec<HttpRequest> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl HttpClient for MockHttpClient {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(req.clone());
        }
        let next = self.responses.lock().ok().and_then(|mut q| q.pop_front());
        next.unwrap_or_else(|| {
            Err(HttpError::Transport {
                method: req.method.as_str().to_string(),
                url: req.url.clone(),
                reason: "mock exhausted: no scripted response".to_string(),
            })
        })
    }
}
