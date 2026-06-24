//! The thin, **synchronous, runtime-free** HTTP transport seam this crate sends auth requests
//! through. The owned request/response DTOs ([`HttpMethod`]/[`HttpRequest`]/[`HttpResponse`]) and
//! the header-redaction authority ([`SENSITIVE_HEADERS`]/[`is_sensitive_header`]) come from the
//! shared **leaf** [`qfs_http_core`] — the **single source of truth** also depended on by
//! `qfs-driver-http`. The transport *trait* itself ([`HttpExchange`]) and its in-memory
//! [`MockExchange`] stay **local** so this crate does not depend on `qfs-driver-http`.
//!
//! ## Why a local seam instead of depending on `qfs-driver-http`
//! `qfs-driver-http` depends on `qfs-runtime` (for the `PlanApplierBridge` + the `EffectError`
//! lowering), and the workspace's confinement test requires **every** `qfs-runtime` consumer to be
//! a *leaf* — no crate may depend back onto it, or tokio could transit out of the runtime, through
//! that crate, and back into the pure spine. If `qfs-google-auth` depended on `qfs-driver-http`,
//! the latter would stop being a leaf and the invariant would break. So this crate keeps its own
//! [`HttpExchange`] trait + [`MockExchange`] (the part with no runtime in it), and the consuming
//! Gmail/Drive/Analytics drivers — themselves runtime leaves already holding an
//! `Arc<dyn qfs_driver_http::HttpClient>` — supply an adapter implementing [`HttpExchange`] over
//! it (a trivial DTO copy). `reqwest` thus stays confined to `qfs-driver-http` exactly as before.
//!
//! ## Single-source DTOs + redaction (t19 refinement)
//! Before the refinement this module hand-copied the t18 DTOs + `SENSITIVE_HEADERS` + the redacting
//! `Debug`, and the copies had already drifted (a 2-variant `HttpMethod` here vs. t18's
//! `#[non_exhaustive]` 4-variant set). The hazard was a **token leak by drift**: if one side added
//! a sensitive header and the other's copy lagged, the lagging adapter would copy that header
//! *value* across the seam and its redaction would silently miss it. The DTOs + redaction now live
//! in [`qfs_http_core`] only; this module re-exports them, so [`qfs_http_core::is_sensitive_header`]
//! is the lone redaction authority for both HTTP seams.

pub use qfs_http_core::{
    is_sensitive_header, HttpMethod, HttpRequest, HttpResponse, SENSITIVE_HEADERS,
};

/// A transport failure before an HTTP status was received — secret-free (built from the request
/// shape only). Mirrors the transport class of the t18 `HttpError`. This stays local to the auth
/// crate: it is the error half of the [`HttpExchange`] seam, not a shared DTO.
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

impl TransportError {
    /// A short, stable code for structured surfaces (mirrors t18 `http_transport`).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "http_transport"
    }
}

/// The thin synchronous transport seam: the OAuth client builds an owned [`HttpRequest`] and calls
/// [`HttpExchange::exchange`]; the implementation performs the wire exchange and returns an owned
/// [`HttpResponse`] (a non-2xx status is **not** an error — it is in the response so the caller can
/// classify a 401/`invalid_grant` body) or a [`TransportError`].
///
/// `Send + Sync` so an `Arc<dyn HttpExchange>` can be shared across the runtime's blocking apply
/// threads. The Gmail/Drive/Analytics drivers implement this over their
/// `Arc<dyn qfs_driver_http::HttpClient>` with a trivial DTO copy.
pub trait HttpExchange: Send + Sync {
    /// Execute one request synchronously.
    ///
    /// # Errors
    /// [`TransportError`] if the wire exchange fails before a status is received.
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl HttpExchange for std::sync::Arc<dyn HttpExchange> {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).exchange(req)
    }
}

/// An in-memory mock transport (tests / CI / wasm): records every request and answers from a FIFO
/// queue of scripted responses — so a test asserts the exact request shape the OAuth client built
/// (method, URL, headers, body) **without any socket**. Mirrors the t18 `MockHttpClient`.
#[derive(Default)]
pub struct MockExchange {
    responses: std::sync::Mutex<std::collections::VecDeque<Result<HttpResponse, TransportError>>>,
    recorded: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockExchange {
    /// An empty mock (every `exchange` after the queue drains returns a terminal transport
    /// error).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one scripted success response (consumed FIFO).
    #[must_use]
    pub fn with_response(self, resp: HttpResponse) -> Self {
        self.push(Ok(resp));
        self
    }

    /// Queue one scripted transport error (consumed FIFO).
    #[must_use]
    pub fn with_error(self, err: TransportError) -> Self {
        self.push(Err(err));
        self
    }

    /// Queue a response after construction.
    pub fn push_response(&self, resp: HttpResponse) {
        self.push(Ok(resp));
    }

    fn push(&self, item: Result<HttpResponse, TransportError>) {
        if let Ok(mut q) = self.responses.lock() {
            q.push_back(item);
        }
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
