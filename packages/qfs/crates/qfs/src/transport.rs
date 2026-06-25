//! The real `reqwest` HTTP transport, hosted in the **`qfs` binary crate** — the one production
//! impl of the per-driver `HttpTransport` seams (github / slack, RFD §9 boundary B3).
//!
//! ## Why it lives here (not in the driver crates)
//! `qfs-driver-github` / `qfs-driver-slack` are deliberately **transport-agnostic**: each declares
//! its own thin `HttpTransport` trait (`send(&HttpRequest) -> Result<HttpResponse, TransportError>`)
//! over the **shared `qfs-http-core` DTOs** and never links `reqwest` — so the drivers stay pure +
//! mockable. The single real wire client (`reqwest`) already lives **confined** in
//! `qfs-driver-http` as [`ReqwestClient`]. This adapter bridges that one client onto both drivers'
//! transport traits, in the terminal binary (the allowlisted runtime/reqwest leaf — tokio + reqwest
//! dead-end here, exactly like the commit interpreter). One adapter serves both drivers because
//! their `HttpRequest`/`HttpResponse` are the *same* `qfs-http-core` types — so `send` is a pure
//! delegate + an error-class remap (no DTO conversion).
//!
//! `ReqwestClient::send` returns `Ok(HttpResponse)` for **any** status (even 4xx/5xx) and only
//! `Err` on a true wire failure (connect / timeout / request / body) — which is exactly the
//! transport-seam contract (the driver interprets the status; the transport reports only wire
//! success/failure). So the remap is faithful: an [`HttpError`] becomes the driver's secret-free
//! `TransportError` (class reason only, never a header value — RFD §10).

use std::sync::Arc;

use qfs_driver_http::{HttpClient, HttpError, HttpRequest, HttpResponse, ReqwestClient};

/// The per-request timeout (seconds) for the production transport. Conservative default; a
/// genuinely hung backend fails closed as a transport timeout rather than blocking the commit.
const TIMEOUT_SECS: u64 = 30;

/// The real `reqwest`-backed transport shared by the github + slack apply drivers. Holds the one
/// confined [`ReqwestClient`]; `Send + Sync` so an `Arc<Self>` is shareable as either driver's
/// `Arc<dyn HttpTransport>`.
pub struct ReqwestTransport {
    inner: ReqwestClient,
}

impl ReqwestTransport {
    /// Build the transport with the default per-request timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: ReqwestClient::new(TIMEOUT_SECS),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a `qfs-driver-http` [`HttpError`] to a secret-free reason string. `HttpError`'s `Display` is
/// machine-facing and credential-free by construction (it carries method + URL + a class reason,
/// never a header value), so it is a safe transport-class reason.
fn reason(err: &HttpError) -> String {
    err.to_string()
}

impl qfs_driver_github::HttpTransport for ReqwestTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, qfs_driver_github::TransportError> {
        self.inner
            .send(req)
            .map_err(|e| qfs_driver_github::TransportError { reason: reason(&e) })
    }
}

impl qfs_driver_slack::HttpTransport for ReqwestTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, qfs_driver_slack::TransportError> {
        self.inner
            .send(req)
            .map_err(|e| qfs_driver_slack::TransportError { reason: reason(&e) })
    }
}

/// A `ReqwestTransport` as the github driver's transport.
#[must_use]
pub fn github_transport() -> Arc<dyn qfs_driver_github::HttpTransport> {
    Arc::new(ReqwestTransport::new())
}

/// A `ReqwestTransport` as the slack driver's transport.
#[must_use]
pub fn slack_transport() -> Arc<dyn qfs_driver_slack::HttpTransport> {
    Arc::new(ReqwestTransport::new())
}

#[cfg(test)]
mod tests {
    //! The adapter is exercised against a **real loopback HTTP server stood up in-process** — a
    //! `std::net::TcpListener` on `127.0.0.1:0` (an ephemeral port) that serves one canned
    //! HTTP/1.1 response and exits. This proves the production `reqwest` transport genuinely
    //! performs the wire exchange (connect → request → status + headers + body) **with NO live
    //! external network** — the same in-process pattern `qfs-driver-http` uses for `ReqwestClient`.
    use super::*;
    use qfs_driver_http::HttpMethod;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Stand up a one-shot loopback server that returns `status` with `body`, and return its
    /// `http://127.0.0.1:<port>/` base URL. The server thread accepts exactly one connection,
    /// reads the request headers, writes the response, and exits.
    fn one_shot_server(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request head (up to the blank line) so the client's write completes.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/")
    }

    #[test]
    fn delegates_a_real_loopback_exchange_and_returns_the_response() {
        let url = one_shot_server(200, "{\"ok\":true}");
        let transport = ReqwestTransport::new();
        let req = HttpRequest::new(HttpMethod::Get, url);

        let resp = qfs_driver_github::HttpTransport::send(&transport, &req)
            .expect("loopback exchange succeeds");

        assert_eq!(
            resp.status, 200,
            "status round-trips from the loopback server"
        );
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            "{\"ok\":true}",
            "body round-trips"
        );
    }

    #[test]
    fn non_2xx_is_a_response_not_a_transport_error() {
        // The transport seam reports wire success/failure only — a 404 is a *response* the driver
        // interprets, never a TransportError. This is the contract the github/slack appliers rely on.
        let url = one_shot_server(404, "not found");
        let transport = ReqwestTransport::new();
        let req = HttpRequest::new(HttpMethod::Get, url);
        let resp = qfs_driver_github::HttpTransport::send(&transport, &req)
            .expect("a 404 is still a successful wire exchange");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn a_dead_address_is_a_secret_free_transport_error() {
        // Nothing is listening on this loopback port → a connect failure surfaces as a
        // class-only TransportError (no header value, no credential).
        let transport = ReqwestTransport::new();
        // Port 1 on loopback: reserved/unbindable, reliably refuses.
        let req = HttpRequest::new(HttpMethod::Get, "http://127.0.0.1:1/");
        let err = qfs_driver_github::HttpTransport::send(&transport, &req)
            .expect_err("a dead address fails the wire exchange");
        assert!(
            !err.reason.is_empty(),
            "transport error carries a class reason"
        );
    }
}
