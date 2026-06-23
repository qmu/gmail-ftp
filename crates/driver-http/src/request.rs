//! The **reusable REST request/response seam** (RFD-0001 §5/§9): owned, vendor-free DTOs
//! that describe one HTTP exchange — [`HttpMethod`], [`HttpRequest`], [`HttpResponse`] — plus
//! the structured, **secret-free** [`HttpError`]. No `reqwest`/`hyper`/`url` type appears in
//! any signature here; the concrete client ([`crate::client`]) trades only in these DTOs.
//!
//! This is the layer t24 (GitHub) and t25 (Slack) build their specific APIs on top of: they
//! construct an [`HttpRequest`] (method + url + headers + body), hand it to an
//! [`crate::client::HttpClient`], and decode the [`HttpResponse`] body through the codec
//! registry — reusing the auth-injection, error-classification, and pagination machinery
//! rather than re-implementing an HTTP path per API.
//!
//! ## Secret discipline (RFD §10)
//! [`HttpRequest`] carries already-resolved header *values* (a token may sit in an
//! `Authorization` header by the time it is on the wire), so its [`fmt::Debug`] is **manual**
//! and **redacts** the value of every sensitive header (see [`SENSITIVE_HEADERS`]). A request
//! is never logged with `{:?}` carrying a live token; the structured request log emits the
//! method + URL + redacted header names only.

use core::fmt;

/// HTTP header names whose *values* are redacted in every `Debug`/log rendering of an
/// [`HttpRequest`] (case-insensitive). Auth material rides in these; their presence is
/// surfaced (the name), their value never is (RFD §10).
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
];

/// Whether a header name carries auth material (case-insensitive match against
/// [`SENSITIVE_HEADERS`]) — the gate the redacting `Debug` and the request log use.
#[must_use]
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// The HTTP method a universal verb maps onto **internally** (RFD §3 "the path is the type":
/// the DSL has no HTTP-verb keywords — this mapping is config/driver-internal). A **closed**
/// set; this ticket maps `SELECT->GET`, `INSERT->POST`, `UPSERT->PUT`, `REMOVE->DELETE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HttpMethod {
    /// `GET` — a read (`SELECT`, `http.get`). Idempotent, retry-safe.
    Get,
    /// `POST` — a create (`INSERT`). **Not** idempotent; never auto-retried (RFD §6).
    Post,
    /// `PUT` — an idempotent create-or-update (`UPSERT`). Retry-safe with an idempotency key.
    Put,
    /// `DELETE` — a removal (`REMOVE`). Irreversible (RFD §10) but idempotent on the wire.
    Delete,
}

impl HttpMethod {
    /// The uppercase wire token (`GET`/`POST`/`PUT`/`DELETE`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }

    /// Whether this method is safe to retry on a transient failure. `POST` is **not**
    /// retry-safe (a timed-out POST may have landed; RFD §6 — never auto-retry POST).
    #[must_use]
    pub const fn is_retry_safe(self) -> bool {
        !matches!(self, HttpMethod::Post)
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One fully-described HTTP request — an **owned DTO** the [`crate::client::HttpClient`]
/// executes. Built by the driver from `(verb, config, secrets, rows)`; carries already-
/// resolved header values, so its `Debug` redacts sensitive headers (see the module docs).
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The HTTP method (mapped from the universal verb).
    pub method: HttpMethod,
    /// The fully-resolved request URL (base + resource path + query string).
    pub url: String,
    /// Header `(name, value)` pairs, in insertion order. Sensitive values are redacted in
    /// `Debug`/logs but present here for the wire send.
    pub headers: Vec<(String, String)>,
    /// The request body bytes, if any (`POST`/`PUT` carry the encoded rows; `GET`/`DELETE`
    /// usually do not).
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// Construct a bodyless request (the `GET`/`DELETE` shape).
    #[must_use]
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Builder: append a header. The value is sent verbatim; it is redacted only in
    /// `Debug`/log surfaces when the name is sensitive.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Builder: set the request body bytes.
    #[must_use]
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    /// The header value for `name` (case-insensitive), if present — used by tests and the
    /// pagination follower to read a response/request header without exposing the vec shape.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Manual, **redacting** `Debug`: emits the method, URL, and header *names* (with sensitive
/// values replaced by the redaction marker), plus the body length — **never** a token and
/// never a raw body. This is the only `Debug` a request gets, so wrapping it in a log line or
/// a `{:?}` dump cannot leak auth material (RFD §10).
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let shown = if is_sensitive_header(k) {
                    cfs_secrets::REDACTED
                } else {
                    v.as_str()
                };
                (k.as_str(), shown)
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.as_ref().map_or(0, Vec::len))
            .finish()
    }
}

/// One HTTP response — an **owned DTO** the client returns. The driver classifies the
/// `status` into success vs. a structured [`crate::error::HttpError`], reads pagination
/// coordinates out of `headers`/`body`, and hands the `body` bytes to the codec.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status code (e.g. 200, 404, 503).
    pub status: u16,
    /// Response header `(name, value)` pairs, in receipt order (carries `Link`/`Set-Cookie`
    /// and the content type the codec is chosen from).
    pub headers: Vec<(String, String)>,
    /// The raw response body bytes (decoded to rows by the codec registry).
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Construct a response.
    #[must_use]
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    /// Builder: append a response header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The header value for `name` (case-insensitive), if present.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether the status is a 2xx success.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// Redacting `Debug` for a response: status, header *names*+values (responses rarely carry
/// the request's auth, but `Set-Cookie` is sensitive so it is redacted too), and body length
/// — never the full body in a default dump.
impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let shown = if is_sensitive_header(k) {
                    cfs_secrets::REDACTED
                } else {
                    v.as_str()
                };
                (k.as_str(), shown)
            })
            .collect();
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}
