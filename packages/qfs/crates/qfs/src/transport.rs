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

/// The shared `reqwest` transport as the **Google** auth seam (`qfs_google_auth::HttpExchange`).
///
/// `qfs-google-auth` is deliberately runtime-free: it declares its OWN thin `HttpExchange` trait
/// (`exchange(&HttpRequest) -> Result<HttpResponse, qfs_google_auth::TransportError>`) over the
/// SAME shared `qfs-http-core` DTOs, so it depends on neither `reqwest` nor `qfs-runtime` (the
/// confinement invariant — a runtime consumer must be a leaf, and `qfs-driver-http` is one). This
/// adapter bridges the one confined [`ReqwestClient`] onto that seam in the terminal binary, exactly
/// like the github/slack impls above — a pure delegate (the `HttpRequest`/`HttpResponse` are the
/// identical `qfs-http-core` types) plus an error-class remap.
///
/// The one shape difference from the github/slack seams: `qfs_google_auth::TransportError` carries
/// `method` + `url` + `reason` (vs. the drivers' `reason`-only error), so the remap re-derives the
/// secret-free request shape from `req` (the method token + the URL are never credentials — the
/// bearer/secret rides a redacted header / form body, never the URL). The `reason` stays the
/// `HttpError` class string (credential-free by construction — RFD §10).
impl qfs_google_auth::HttpExchange for ReqwestTransport {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, qfs_google_auth::TransportError> {
        self.inner
            .send(req)
            .map_err(|e| qfs_google_auth::TransportError {
                method: req.method.as_str().to_string(),
                url: req.url.clone(),
                reason: reason(&e),
            })
    }
}

/// The shared `reqwest` transport as the **object-storage** (S3 / R2) backend's wire seam
/// (`qfs_driver_objstore::HttpExchange`).
///
/// `qfs-driver-objstore` is a self-contained runtime leaf that rides its OWN thin `HttpExchange`
/// trait (`exchange(&HttpRequest) -> Result<HttpResponse, qfs_driver_objstore::TransportError>`)
/// over the SAME shared `qfs-http-core` DTOs, so it links neither `reqwest` nor `qfs-driver-http`
/// (the SigV4 signer + the `HttpBackend` stay vendor-free — the `qfs-google-auth` / `qfs-driver-cf`
/// precedent). This adapter bridges the one confined [`ReqwestClient`] onto that seam in the
/// terminal binary, exactly like the github/slack/google impls above — a pure delegate (the
/// `HttpRequest`/`HttpResponse` are the identical `qfs-http-core` types) plus an error-class remap.
///
/// Like the Google seam, `qfs_driver_objstore::TransportError` carries `method` + `url` + `reason`
/// (vs. the drivers' `reason`-only error), so the remap re-derives the secret-free request shape
/// from `req` (the method token + the URL are never credentials — the SigV4 signature rides the
/// `Authorization` header, which `qfs-http-core` redacts in every `Debug`/log). A non-2xx status is
/// a *response* the backend classifies, never a `TransportError` — the same seam contract as above.
impl qfs_driver_objstore::HttpExchange for ReqwestTransport {
    fn exchange(
        &self,
        req: &HttpRequest,
    ) -> Result<HttpResponse, qfs_driver_objstore::TransportError> {
        self.inner
            .send(req)
            .map_err(|e| qfs_driver_objstore::TransportError {
                method: req.method.as_str().to_string(),
                url: req.url.clone(),
                reason: reason(&e),
            })
    }
}

/// The shared `reqwest` transport as the object-storage [`HttpBackend`](qfs_driver_objstore::HttpBackend)
/// wire seam (`qfs_driver_objstore::HttpExchange`) — the one confined wire client the live `/s3` and
/// `/r2` SigV4 backends send their already-signed requests over.
#[must_use]
pub fn objstore_exchange() -> Arc<dyn qfs_driver_objstore::HttpExchange> {
    Arc::new(ReqwestTransport::new())
}

/// A `ReqwestTransport` as the slack driver's transport.
#[must_use]
pub fn slack_transport() -> Arc<dyn qfs_driver_slack::HttpTransport> {
    Arc::new(ReqwestTransport::new())
}

/// A `ReqwestTransport` as the Google auth seam (`qfs_google_auth::HttpExchange`), shared by the
/// OAuth token client and the authenticated `GoogleApiClient` that the gmail/gdrive/ga drivers
/// issue API calls through (the same `Arc` feeds both, so one wire client serves the whole Google
/// stack).
#[must_use]
pub fn google_transport() -> Arc<dyn qfs_google_auth::HttpExchange> {
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
    fn google_exchange_delegates_a_real_loopback_round_trip() {
        // The Google `HttpExchange` seam rides the SAME confined reqwest client as github/slack —
        // prove it performs the real wire exchange over a loopback server (connect → request →
        // status + body) with NO live network, using the `qfs_google_auth` re-exported DTOs.
        use qfs_google_auth::{HttpExchange, HttpMethod as GMethod, HttpRequest as GRequest};
        let url = one_shot_server(200, "{\"email\":\"a@example.com\"}");
        let transport = ReqwestTransport::new();
        let req = GRequest::new(GMethod::Get, url);

        let resp = HttpExchange::exchange(&transport, &req).expect("loopback exchange succeeds");

        assert_eq!(
            resp.status, 200,
            "status round-trips from the loopback server"
        );
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            "{\"email\":\"a@example.com\"}",
            "body round-trips"
        );
    }

    #[test]
    fn google_non_2xx_is_a_response_not_a_transport_error() {
        // A 401 is the load-bearing status for the Google client (it triggers a token refresh +
        // retry), so the seam MUST surface it as a *response*, never a TransportError.
        use qfs_google_auth::{HttpExchange, HttpMethod as GMethod, HttpRequest as GRequest};
        let url = one_shot_server(401, "unauthorized");
        let transport = ReqwestTransport::new();
        let req = GRequest::new(GMethod::Get, url);
        let resp = HttpExchange::exchange(&transport, &req)
            .expect("a 401 is still a successful wire exchange");
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn google_dead_address_is_a_secret_free_transport_error_with_request_shape() {
        // A dead loopback port fails the wire exchange → a class-only TransportError carrying the
        // secret-free request shape (method + URL + class reason), never a credential.
        use qfs_google_auth::{HttpExchange, HttpMethod as GMethod, HttpRequest as GRequest};
        let transport = ReqwestTransport::new();
        let req = GRequest::new(GMethod::Get, "http://127.0.0.1:1/");
        let err = HttpExchange::exchange(&transport, &req)
            .expect_err("a dead address fails the exchange");
        assert_eq!(
            err.method, "GET",
            "the request method rides the error shape"
        );
        assert_eq!(
            err.url, "http://127.0.0.1:1/",
            "the URL rides the error shape"
        );
        assert!(
            !err.reason.is_empty(),
            "transport error carries a class reason"
        );
    }

    #[test]
    fn objstore_exchange_delegates_a_real_loopback_round_trip() {
        // The object-storage `HttpExchange` seam rides the SAME confined reqwest client as
        // github/slack/google — prove it performs the real wire exchange over a loopback server
        // (connect → request → status + body) with NO live network, using the `qfs_driver_objstore`
        // re-exported DTOs. This is the seam the live `/s3` + `/r2` SigV4 backends send over.
        use qfs_driver_objstore::HttpExchange;
        let url = one_shot_server(200, "{\"ok\":true}");
        let transport = ReqwestTransport::new();
        let req = HttpRequest::new(HttpMethod::Get, url);

        let resp = HttpExchange::exchange(&transport, &req).expect("loopback exchange succeeds");

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
    fn objstore_non_2xx_is_a_response_not_a_transport_error() {
        // A 404/403 is the load-bearing status the SigV4 backend classifies (a missing key / an
        // access denial), so the seam MUST surface it as a *response*, never a TransportError.
        use qfs_driver_objstore::HttpExchange;
        let url = one_shot_server(403, "AccessDenied");
        let transport = ReqwestTransport::new();
        let req = HttpRequest::new(HttpMethod::Get, url);
        let resp = HttpExchange::exchange(&transport, &req)
            .expect("a 403 is still a successful wire exchange");
        assert_eq!(resp.status, 403);
    }

    #[test]
    fn objstore_dead_address_is_a_secret_free_transport_error_with_request_shape() {
        // A dead loopback port fails the wire exchange → a class-only TransportError carrying the
        // secret-free request shape (method + URL + class reason), never a credential.
        use qfs_driver_objstore::HttpExchange;
        let transport = ReqwestTransport::new();
        let req = HttpRequest::new(HttpMethod::Get, "http://127.0.0.1:1/");
        let err = HttpExchange::exchange(&transport, &req)
            .expect_err("a dead address fails the exchange");
        assert_eq!(
            err.method, "GET",
            "the request method rides the error shape"
        );
        assert_eq!(
            err.url, "http://127.0.0.1:1/",
            "the URL rides the error shape"
        );
        assert!(
            !err.reason.is_empty(),
            "transport error carries a class reason"
        );
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
