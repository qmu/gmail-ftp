//! `qfs-http-core` — the shared, **pure leaf** that owns the vendor-free HTTP exchange DTOs and
//! the **single** header-redaction authority (RFD-0001 §5/§10, t19 refinement).
//!
//! ## Why this crate exists — single-source the redaction set (the drift hazard it closes)
//! Two crates speak owned HTTP DTOs across a synchronous transport seam: `qfs-driver-http` (the
//! generic REST driver, with the real `reqwest` [`HttpClient`]) and `qfs-google-auth` (the OAuth
//! base, with a local `HttpExchange` seam kept off the runtime). Before this crate each
//! hand-copied [`HttpMethod`]/[`HttpRequest`]/[`HttpResponse`], the [`SENSITIVE_HEADERS`] list,
//! and the redacting `Debug` — and the copies had **already drifted** (a 2-variant `HttpMethod`
//! on one side vs. the 4-variant `#[non_exhaustive]` set on the other). The danger is not the
//! method enum itself: it is that if one crate adds a sensitive header and the other's copy
//! lags, the lagging adapter copies that header *value* across the seam and its redaction
//! silently misses it — **a token leak by drift**. Centralizing the DTOs **and** the redaction
//! predicate here makes [`is_sensitive_header`] the lone authority; a new sensitive header is
//! added in exactly one place and both crates inherit it.
//!
//! ## A pure leaf — no reqwest / tokio / runtime
//! This crate carries owned DTOs and the redaction logic only. Its sole workspace dependency is
//! `qfs-secrets` (for the one [`qfs_secrets::REDACTED`] marker the manual `Debug` impls emit), so
//! the dep closure is `qfs-http-core → qfs-secrets → qfs-types` — no `reqwest`, no `tokio`, no
//! `qfs-runtime`. The concrete transports stay where they belong: `qfs-driver-http` keeps its
//! `HttpClient` trait + `ReqwestClient`/`MockHttpClient`; `qfs-google-auth` keeps its
//! `HttpExchange` trait + `MockExchange`. Both trade **only** in the DTOs defined here.
//!
//! ## Secret discipline (RFD §10)
//! [`HttpRequest`] carries already-resolved header *values* (a bearer token may sit in an
//! `Authorization` header by the time it is on the wire), so its [`fmt::Debug`] is **manual** and
//! **redacts** the value of every sensitive header (see [`SENSITIVE_HEADERS`]). A request is never
//! logged with `{:?}` carrying a live token; the structured request log emits the method + URL +
//! redacted header names only. [`HttpResponse`]'s `Debug` redacts too (`Set-Cookie` is sensitive).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use core::fmt;

/// HTTP header names whose *values* are redacted in every `Debug`/log rendering of an
/// [`HttpRequest`]/[`HttpResponse`] (case-insensitive). Auth material rides in these; their
/// presence is surfaced (the name), their value never is (RFD §10).
///
/// This is the **single source of truth** for the workspace's redaction set. Adding a sensitive
/// header here (and nowhere else) propagates the redaction to both the HTTP/REST driver and the
/// Google auth base — neither carries a second copy that could drift out of date and leak a value.
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
/// [`SENSITIVE_HEADERS`]) — **the** gate the redacting `Debug` impls and every request log use.
/// This is the lone redaction authority: there is no second copy of this predicate in the
/// workspace, so a header is sensitive iff this function says so.
#[must_use]
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// The HTTP method a universal verb maps onto **internally** (RFD §3 "the path is the type": the
/// DSL has no HTTP-verb keywords — this mapping is config/driver-internal). A **closed**,
/// `#[non_exhaustive]` set: `SELECT→GET`, `INSERT→POST`, `UPSERT→PUT`, `REMOVE→DELETE`, and the
/// partial-update `PATCH` (`UPDATE … SET …`). The OAuth flow uses only `Get` (userinfo) and
/// `Post` (token endpoint); the REST driver uses GET/POST/PUT/DELETE; the GitHub (t24) and
/// Drive (t21) drivers also use `Patch` for partial field edits — one reconciled enum serves all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HttpMethod {
    /// `GET` — a read (`SELECT`, `http.get`, the OAuth userinfo lookup). Idempotent, retry-safe.
    Get,
    /// `POST` — a create (`INSERT`, the OAuth token exchange/refresh). **Not** idempotent; never
    /// auto-retried (RFD §6).
    Post,
    /// `PUT` — an idempotent create-or-update (`UPSERT`). Retry-safe with an idempotency key.
    Put,
    /// `PATCH` — a partial update (`UPDATE … SET …`, e.g. GitHub `state='closed'`, a Drive
    /// metadata rename). Per RFC 7231 `PATCH` is **not** guaranteed idempotent, so it is treated
    /// as **not** retry-safe (a timed-out PATCH may have applied; RFD §6 — do not auto-retry).
    Patch,
    /// `DELETE` — a removal (`REMOVE`). Irreversible (RFD §10) but idempotent on the wire.
    Delete,
}

impl HttpMethod {
    /// The uppercase wire token (`GET`/`POST`/`PUT`/`PATCH`/`DELETE`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }

    /// Whether this method is safe to retry on a transient failure. `POST` and `PATCH` are
    /// **not** retry-safe (a timed-out POST may have landed; a `PATCH` is not guaranteed
    /// idempotent — RFD §6, never auto-retry either).
    #[must_use]
    pub const fn is_retry_safe(self) -> bool {
        !matches!(self, HttpMethod::Post | HttpMethod::Patch)
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One fully-described HTTP request — an **owned DTO** a concrete client/exchange executes. Built
/// from `(verb, config, secrets, rows)` (REST) or by the OAuth client; carries already-resolved
/// header values, so its `Debug` redacts sensitive headers (see the module docs).
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
    /// pagination follower to read a header without exposing the vec shape.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Manual, **redacting** `Debug`: emits the method, URL, and header *names* (with sensitive
/// values replaced by the redaction marker), plus the body length — **never** a token and never a
/// raw body. This is the only `Debug` a request gets, so wrapping it in a log line or a `{:?}`
/// dump cannot leak auth material (RFD §10).
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let shown = if is_sensitive_header(k) {
                    qfs_secrets::REDACTED
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

/// One HTTP response — an **owned DTO** a concrete client/exchange returns. The driver classifies
/// `status` into success vs. a structured error, reads pagination coordinates out of
/// `headers`/`body`, and hands the `body` bytes to the codec.
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

/// Redacting `Debug` for a response: status, header *names*+values (responses rarely carry the
/// request's auth, but `Set-Cookie` is sensitive so it is redacted too), and body length — never
/// the full body in a default dump.
impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let shown = if is_sensitive_header(k) {
                    qfs_secrets::REDACTED
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_render_uppercase_tokens_and_post_is_not_retry_safe() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Post.as_str(), "POST");
        assert_eq!(HttpMethod::Put.as_str(), "PUT");
        assert_eq!(HttpMethod::Patch.as_str(), "PATCH");
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
        assert!(HttpMethod::Get.is_retry_safe());
        assert!(HttpMethod::Put.is_retry_safe());
        assert!(HttpMethod::Delete.is_retry_safe());
        assert!(!HttpMethod::Post.is_retry_safe());
        // PATCH is not guaranteed idempotent → not retry-safe (RFD §6).
        assert!(!HttpMethod::Patch.is_retry_safe());
        assert_eq!(format!("{}", HttpMethod::Delete), "DELETE");
        assert_eq!(format!("{}", HttpMethod::Patch), "PATCH");
    }

    #[test]
    fn sensitive_headers_are_matched_case_insensitively() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("AUTHORIZATION"));
        assert!(is_sensitive_header("set-cookie"));
        assert!(is_sensitive_header("X-Api-Key"));
        assert!(!is_sensitive_header("content-type"));
        assert!(!is_sensitive_header("link"));
    }

    #[test]
    fn request_debug_redacts_sensitive_header_values() {
        let req = HttpRequest::new(HttpMethod::Get, "https://api/x")
            .header("Authorization", "Bearer super-secret-token")
            .header("Content-Type", "application/json")
            .with_body(b"hello".to_vec());
        let dbg = format!("{req:?}");
        assert!(
            !dbg.contains("super-secret-token"),
            "token must never appear in Debug: {dbg}"
        );
        assert!(
            dbg.contains(qfs_secrets::REDACTED),
            "redaction marker present: {dbg}"
        );
        // Non-sensitive header values are shown verbatim, and the body is summarized by length.
        assert!(dbg.contains("application/json"));
        assert!(dbg.contains("body_len"));
        assert!(!dbg.contains("hello"));
    }

    #[test]
    fn response_debug_redacts_set_cookie() {
        let resp = HttpResponse::new(200, b"body".to_vec())
            .header("Set-Cookie", "session=abc123-secret")
            .header("Content-Type", "text/plain");
        let dbg = format!("{resp:?}");
        assert!(
            !dbg.contains("abc123-secret"),
            "Set-Cookie value must be redacted: {dbg}"
        );
        assert!(dbg.contains(qfs_secrets::REDACTED));
        assert!(resp.is_success());
    }

    #[test]
    fn header_value_lookup_is_case_insensitive() {
        let req = HttpRequest::new(HttpMethod::Post, "https://api/x").header("X-Trace", "42");
        assert_eq!(req.header_value("x-trace"), Some("42"));
        assert_eq!(req.header_value("missing"), None);
    }
}
