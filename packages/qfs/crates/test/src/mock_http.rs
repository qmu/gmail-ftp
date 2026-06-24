//! A wasm-clean **scripted** mock HTTP transport (t38, RFD §5/§9): match request → canned
//! response, record every request, no socket.
//!
//! ## Why a small in-house transport, not `httptest`/`wiremock` (ADR-0006)
//! `httptest`/`wiremock` are absent from the offline cargo cache **and** are socket-bound
//! (they bind a real loopback listener), so neither is affordable on the tight disk nor usable
//! in the no-socket, wasm-pure harness. The driver tickets all hand-rolled a scripted, in-
//! memory transport instead (`qfs-driver-http::MockHttpClient` is the canonical one). This is
//! that pattern, lifted to a wasm-clean leaf built only on the pure [`qfs_http_core`] DTOs —
//! the real reqwest [`HttpClient`] stays in `qfs-driver-http`, never pulled here.
//!
//! The transport is scripted FIFO: queue canned responses, drive the driver, then assert the
//! recorded requests (method/url/headers/body) — the recorded auth headers prove the driver
//! injects credentials, and the redacting [`qfs_http_core::HttpRequest`] `Debug` proves they
//! are never logged.

use std::cell::RefCell;
use std::collections::VecDeque;

use qfs_http_core::{HttpRequest, HttpResponse};

/// A scripted, in-memory HTTP transport: canned responses consumed FIFO, every request
/// recorded. Single-threaded (`RefCell`) so it stays wasm-clean (no `Mutex`/threads); the
/// pure parse/plan/codec subset never needs `Send`. A test queues responses, hands the mock to
/// the code under test, then inspects [`MockHttp::recorded`].
#[derive(Debug, Default)]
pub struct MockHttp {
    responses: RefCell<VecDeque<HttpResponse>>,
    recorded: RefCell<Vec<HttpRequest>>,
}

impl MockHttp {
    /// An empty mock (every `send` after the queue drains is a scripting error — see `send`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one scripted response (builder; consumed FIFO).
    #[must_use]
    pub fn with_response(self, resp: HttpResponse) -> Self {
        self.responses.borrow_mut().push_back(resp);
        self
    }

    /// Queue a response after construction (the multi-page pagination helper).
    pub fn push_response(&self, resp: HttpResponse) {
        self.responses.borrow_mut().push_back(resp);
    }

    /// Record `req` and return the next scripted response, or a transport-style error response
    /// (HTTP 599 + a clear body) when the script is exhausted — a drained queue is a test-
    /// scripting bug surfaced as a recognizable status rather than a panic, so the driver's own
    /// error path is exercised.
    pub fn send(&self, req: &HttpRequest) -> HttpResponse {
        self.recorded.borrow_mut().push(req.clone());
        self.responses.borrow_mut().pop_front().unwrap_or_else(|| {
            HttpResponse::new(599, b"mock exhausted: no scripted response".to_vec())
        })
    }

    /// The requests this mock received, in order — what a test asserts against (method, url,
    /// headers, body). Returns owned clones so the test holds a stable snapshot.
    #[must_use]
    pub fn recorded(&self) -> Vec<HttpRequest> {
        self.recorded.borrow().clone()
    }

    /// How many requests the mock has received.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.recorded.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_http_core::HttpMethod;

    #[test]
    fn scripts_responses_fifo_and_records_requests() {
        let mock = MockHttp::new()
            .with_response(HttpResponse::new(200, b"{\"page\":1}".to_vec()))
            .with_response(HttpResponse::new(200, b"{\"page\":2}".to_vec()));

        let r1 = mock.send(&HttpRequest::new(HttpMethod::Get, "https://x/1"));
        let r2 = mock.send(&HttpRequest::new(HttpMethod::Get, "https://x/2"));
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body, b"{\"page\":1}");
        assert_eq!(r2.body, b"{\"page\":2}");

        let rec = mock.recorded();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].url, "https://x/1");
        assert_eq!(rec[1].method, HttpMethod::Get);
    }

    #[test]
    fn exhausted_script_returns_a_recognizable_error_status() {
        let mock = MockHttp::new();
        let resp = mock.send(&HttpRequest::new(HttpMethod::Get, "https://x"));
        assert_eq!(resp.status, 599, "drained queue is a scripting error");
    }

    #[test]
    fn auth_header_is_recorded_but_never_logged() {
        // A request carrying an Authorization header: the mock records it (so a test can prove
        // the driver injected auth), but the redacting Debug never prints the value (RFD §10).
        let mock = MockHttp::new().with_response(HttpResponse::new(200, Vec::new()));
        let req = HttpRequest::new(HttpMethod::Get, "https://x")
            .header("authorization", "Bearer ya29.secret-token");
        let _ = mock.send(&req);

        let recorded = &mock.recorded()[0];
        // The header value IS present in the owned DTO (the driver injected it)...
        assert_eq!(
            recorded.header_value("authorization"),
            Some("Bearer ya29.secret-token")
        );
        // ...but the Debug rendering redacts it — it never reaches a log.
        let debug = format!("{recorded:?}");
        assert!(
            !debug.contains("ya29.secret-token"),
            "redacting Debug must not leak the token: {debug}"
        );
    }
}
