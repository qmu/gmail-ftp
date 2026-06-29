//! The native HTTP/1.1 listener (t32): a minimal in-house server over
//! [`tokio::net::TcpListener`], NO axum (see `docs/adr/0004-http-serving.md` — axum is not in
//! the offline cache and disk is constrained; the cached tokio suffices for the endpoint
//! contract).
//!
//! This is the ONLY native-specific shim in the crate. It parses an HTTP/1.1 request into the
//! owned [`HttpRequest`] DTO, dispatches it through the vendor-free pipeline
//! ([`crate::route::Router`] + [`crate::handler::dispatch`]), and serializes the owned
//! [`HttpResponse`] back to wire bytes. A Cloudflare Worker `fetch` (E7/t35) would replace
//! THIS file and reuse everything else unchanged.
//!
//! Scope (the endpoint contract, not a general server): request line + headers + a bounded
//! body, method+route match, JSON/CSV response, status codes. Pipelining, chunked transfer,
//! and keep-alive beyond one request are out of scope (a follow-up); each connection serves
//! one request then closes — sufficient for the read-endpoint contract and the loopback tests.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};

use qfs_core::Engine;
use qfs_exec::ReadRegistry;
use qfs_server::Runtime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::binding::HttpBinding;
use crate::handler::{dispatch, EndpointCtx};
use crate::route::Router;
use crate::{HttpRequest, HttpResponse, Method};

/// The default loopback bind address for `qfs serve`. Loopback-only by default (RFD §10: a
/// trusted bind address; auth for callers is E5/E8). Overridable via `QFS_HTTP_ADDR`.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8787";

/// Boot `config` and run the HTTP serving binding under the [`Runtime`] supervisor (t32) — the
/// `qfs serve` composition root the binary calls.
///
/// Wires a [`HttpBinding`] (over `engine` + `reads`) into the runtime, boots the `.qfs` config
/// (which reconciles the binding's route table from `/server/endpoints`), binds the HTTP
/// listener on `addr`, and runs the listener concurrently with the runtime's supervised
/// `ctrl_c` wait. A committed `/server/endpoints` mutation re-reconciles the binding (hot
/// route swap) for the next request. Returns when `ctrl_c` fires.
///
/// `addr` is loopback by default ([`DEFAULT_BIND_ADDR`]); the caller may override it. No live
/// network is required to boot — only to ACCEPT requests (which the loopback listener serves).
///
/// # Errors
/// A boxed error if boot fails, the listener cannot bind, or the runtime wiring fails.
pub async fn serve_config(
    config: &Path,
    engine: Arc<Engine>,
    reads: Arc<ReadRegistry>,
    addr: SocketAddr,
    max_rows: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_config_with(config, engine, reads, addr, max_rows, Vec::new()).await
}

/// A request **fallback handler** (t34): a synchronous closure the listener invokes when the
/// endpoint router has NO match for a request. The `qfs` binary wires the watchtower's webhook
/// `ingest` here for `/hooks/...` paths — so `qfs-http` gains NO dependency on `qfs-watchtower`
/// (they cross only through the owned [`HttpRequest`]/[`HttpResponse`] DTOs + this closure, option
/// b of the t34 webhook-serving decision). `None` means "I don't handle this path" → the usual
/// 404. The closure is `Fn` (re-entrant, shared across connections) + `Send + Sync`.
pub type Fallback = Arc<dyn Fn(&HttpRequest) -> Option<HttpResponse> + Send + Sync>;

/// Like [`serve_config`], but also registers `extra_bindings` (e.g. the t33 cron `CronBinding`)
/// into the runtime alongside the HTTP binding. The binary's serve composition root builds the
/// cron binding (a sibling leaf) and passes it here as a `Box<dyn qfs_server::Binding>` — so
/// `qfs-http` gains no dependency on `qfs-cron` (the bindings cross only through the generic
/// `Binding` trait), keeping both leaves independent.
///
/// # Errors
/// A boxed error if boot fails, the listener cannot bind, or the runtime wiring fails.
pub async fn serve_config_with(
    config: &Path,
    engine: Arc<Engine>,
    reads: Arc<ReadRegistry>,
    addr: SocketAddr,
    max_rows: usize,
    extra_bindings: Vec<Box<dyn qfs_server::Binding>>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_config_full(config, engine, reads, addr, max_rows, extra_bindings, None).await
}

/// Like [`serve_config_with`], but ALSO accepts an optional request [`Fallback`] the listener
/// invokes when the endpoint router has no match (t34: the watchtower webhook ingest for
/// `/hooks/...`). The binary composes both routers into ONE listener here; `qfs-http` stays free of
/// any `qfs-watchtower` dependency.
///
/// # Errors
/// A boxed error if boot fails, the listener cannot bind, or the runtime wiring fails.
pub async fn serve_config_full(
    config: &Path,
    engine: Arc<Engine>,
    reads: Arc<ReadRegistry>,
    addr: SocketAddr,
    max_rows: usize,
    extra_bindings: Vec<Box<dyn qfs_server::Binding>>,
    fallback: Option<Fallback>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Construct the binding and capture its shared router handle BEFORE boxing it into the
    // runtime (the listener reads the same atomically-swapped table the runtime reconciles).
    let binding = HttpBinding::new(Arc::clone(&engine), Arc::clone(&reads), max_rows);
    let router = binding.router_handle();
    let ctx = binding.ctx();

    let mut runtime = Runtime::new().with_binding(Box::new(binding));
    for extra in extra_bindings {
        runtime = runtime.with_binding(extra);
    }
    runtime.boot(config)?;

    // The runtime's `run` OWNS the single `ctrl_c` shutdown + the audit drain — it must run to
    // completion on shutdown (a `select!` race could drop it un-drained). So we spawn the
    // listener on a `watch`-channel shutdown, await `runtime.run()` (which drains on ctrl_c),
    // THEN signal the listener to stop and join it. The listener never owns `ctrl_c`.
    // Bind the listener up front so a port conflict is observable HERE (before spawning) and
    // can be treated as NON-FATAL to boot: the config boot + audit drain are the core of
    // `qfs serve` (RFD §8 — boot needs no network), and the HTTP listener is a binding atop it.
    // A bind failure logs a warning and the runtime runs without the listener, rather than
    // aborting the whole process (so two `serve` instances, or a taken port, do not break boot).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let listener_handle = match TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!(target: "qfs::http", %addr, "http listener bound");
            let mut rx = shutdown_rx;
            let fallback = fallback.clone();
            Some(tokio::spawn(async move {
                let wait_shutdown = async move {
                    while rx.changed().await.is_ok() {
                        if *rx.borrow() {
                            break;
                        }
                    }
                };
                serve_on_with(listener, router, ctx, fallback, wait_shutdown).await;
            }))
        }
        Err(e) => {
            tracing::warn!(
                target: "qfs::http",
                %addr,
                error = %e,
                "http listener could not bind; serving config only (boot continues)"
            );
            None
        }
    };

    // Block in the runtime's supervised wait; it drains the audit ledger on `ctrl_c`.
    let run_result = runtime.run().await;

    // Tell the listener (if it bound) to stop and join it (best-effort — a join error never
    // masks the run result, which carries the audit-drain outcome the e2e contract observes).
    let _ = shutdown_tx.send(true);
    if let Some(handle) = listener_handle {
        let _ = handle.await;
    }
    run_result?;
    Ok(())
}

/// The maximum request size the in-house parser accepts (header + body). A bound so a single
/// connection cannot exhaust memory (RFD §6 resource discipline); a larger request is rejected.
const MAX_REQUEST_BYTES: usize = 1 << 20; // 1 MiB

/// Bind `addr` and serve endpoint requests until `shutdown` resolves. The `router` handle is
/// the binding's atomically swapped route table (so a hot reconcile re-binds live routes); the
/// `ctx` supplies the engine / reads / policies / result cap.
///
/// Binds to a loopback / supplied `SocketAddr` only — the caller chooses the address (the
/// tests bind `127.0.0.1:0`; no system port). Returns when `shutdown` fires.
///
/// # Errors
/// An `std::io::Error` if the listener cannot bind the address.
pub async fn serve<F>(
    addr: SocketAddr,
    router: Arc<RwLock<Arc<Router>>>,
    ctx: EndpointCtx,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(target: "qfs::http", %addr, "http listener bound");
    serve_on(listener, router, ctx, shutdown).await;
    Ok(())
}

/// Run the accept loop over an ALREADY-BOUND listener until `shutdown` resolves. Split from
/// [`serve`] so the composition root ([`serve_config`]) can bind first (making a port conflict
/// observable + non-fatal) and then run the loop. Each accepted connection serves one request.
pub async fn serve_on<F>(
    listener: TcpListener,
    router: Arc<RwLock<Arc<Router>>>,
    ctx: EndpointCtx,
    shutdown: F,
) where
    F: std::future::Future<Output = ()> + Send,
{
    serve_on_with(listener, router, ctx, None, shutdown).await;
}

/// Like [`serve_on`], but ALSO consults an optional request [`Fallback`] when the router has no
/// match (t34: the watchtower webhook ingest). The fallback runs BEFORE the 404 — if it returns
/// `Some(resp)`, that is the response; otherwise the request 404s as usual.
pub async fn serve_on_with<F>(
    listener: TcpListener,
    router: Arc<RwLock<Arc<Router>>>,
    ctx: EndpointCtx,
    fallback: Option<Fallback>,
    shutdown: F,
) where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!(target: "qfs::http", "http listener shutting down");
                return;
            }
            accepted = listener.accept() => {
                let (mut stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(target: "qfs::http", error = %e, "accept failed");
                        continue;
                    }
                };
                let router = Arc::clone(&router);
                let ctx = ctx.clone();
                let fallback = fallback.clone();
                tokio::spawn(async move {
                    let resp = match read_request(&mut stream).await {
                        Ok(Some(req)) => handle_with(&router, &ctx, fallback.as_ref(), &req).await,
                        // A malformed / oversized / empty request → a minimal 400; never panic.
                        Ok(None) => HttpResponse::new(400, "application/json",
                            br#"{"error":"bind","detail":"malformed request"}"#.to_vec()),
                        Err(_) => HttpResponse::new(400, "application/json",
                            br#"{"error":"bind","detail":"malformed request"}"#.to_vec()),
                    };
                    let _ = stream.write_all(&serialize_response(&resp)).await;
                    let _ = stream.flush().await;
                });
            }
        }
    }
}

/// Dispatch one parsed request against the live router snapshot. Reads the router pointer by
/// cloning the `Arc` under a momentary guard (never held across the `.await`).
/// Dispatch one parsed request: a matched endpoint route always wins; on a router MISS the
/// optional `fallback` (the t34 watchtower webhook ingest) gets a chance before the 404.
async fn handle_with(
    router: &Arc<RwLock<Arc<Router>>>,
    ctx: &EndpointCtx,
    fallback: Option<&Fallback>,
    req: &HttpRequest,
) -> HttpResponse {
    let snapshot = match router.read() {
        Ok(g) => Arc::clone(&g),
        Err(_) => {
            return HttpResponse::new(
                500,
                "application/json",
                br#"{"error":"internal","detail":"router lock poisoned"}"#.to_vec(),
            )
        }
    };
    match snapshot.match_request(&req.method, &req.path) {
        Some((route, path_params)) => dispatch(route, path_params, req, ctx).await,
        None => {
            // Router miss: give the fallback (the watchtower webhook ingest) a chance before 404.
            if let Some(fb) = fallback {
                if let Some(resp) = fb(req) {
                    return resp;
                }
            }
            crate::error::HttpError::NotFound.into_response()
        }
    }
}

/// Read + parse one HTTP/1.1 request from the stream into an owned [`HttpRequest`]. Returns
/// `Ok(None)` for an empty / malformed / oversized request (the caller renders a 400). Reads
/// up to [`MAX_REQUEST_BYTES`]; honours `Content-Length` for the body.
async fn read_request<S>(stream: &mut S) -> std::io::Result<Option<HttpRequest>>
where
    S: AsyncReadExt + Unpin,
{
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    // Read until we have the full header block (\r\n\r\n) or hit the size bound.
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // EOF before a complete header block.
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let request_line = match lines.next() {
        Some(l) if !l.is_empty() => l,
        _ => return Ok(None),
    };
    let (method, target) = match parse_request_line(request_line) {
        Some(parts) => parts,
        None => return Ok(None),
    };
    let (path, query) = split_target(&target);

    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // Read the body up to Content-Length (bounded).
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let body_start = header_end + 4; // skip the \r\n\r\n
    let mut body = if buf.len() > body_start {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };
    while body.len() < content_length {
        if body.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(HttpRequest {
        method: Method::parse(&method),
        path,
        query,
        headers,
        body,
    }))
}

/// Serialize an owned [`HttpResponse`] into HTTP/1.1 wire bytes. `Connection: close` (one
/// request per connection — the t32 scope).
fn serialize_response(resp: &HttpResponse) -> Vec<u8> {
    let reason = status_reason(resp.status);
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        reason,
        resp.content_type,
        resp.body.len(),
    );
    // Extra headers (Location for a redirect, Set-Cookie for a session) emitted verbatim. Values are
    // single-line (no CR/LF) by construction here; a stray newline would be a header-injection
    // vector, so strip any CR/LF defensively before writing it onto the wire.
    for (name, value) in &resp.headers {
        let safe_value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        head.push_str(&format!("{name}: {safe_value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&resp.body);
    out
}

/// The byte offset of the `\r\n\r\n` header terminator, if present.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the request line `METHOD target HTTP/1.1` into `(method, target)`.
fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target))
}

/// Split a request target into `(path, query_map)`.
fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    match target.split_once('?') {
        Some((path, qs)) => (path.to_string(), parse_query_string(qs)),
        None => (target.to_string(), BTreeMap::new()),
    }
}

/// Parse a `a=1&b=2` query string into a map (last-wins). Minimal `+`/`%XX` decoding.
fn parse_query_string(qs: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(decode(k), decode(v));
    }
    out
}

/// Minimal percent / plus decode (shared shape with the handler's body decoder).
fn decode(s: &str) -> String {
    let spaced = s.replace('+', " ");
    let raw = spaced.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(byte) = u8::from_str_radix(&spaced[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The reason phrase for the status codes this binding emits.
fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Status",
    }
}
