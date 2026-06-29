//! Request/response **multiplexing** over the one outbound connection, plus the in-memory transport
//! the hermetic tests drive it through.
//!
//! The whole point of the fabric is that ONE long-lived outbound connection carries MANY inbound
//! requests — the resident node never opens a port, so every request the relay routes shares the
//! single tunnel. Each logical request/response is a [`crate::TunnelFrame`] stream keyed by
//! `stream_id`; frames for different streams **interleave** freely on the wire and are demuxed by
//! that key.
//!
//! This module provides:
//!   - [`FrameTransport`] — the thin, **synchronous, runtime-free** seam over which whole frames are
//!     sent/received. The live implementation (a tokio socket to qfs Cloud) is a documented seam in
//!     the `qfs` binary; here lives the hermetic [`InMemoryDuplex`] that frames bytes through the
//!     real [`crate::frame`] codec with no socket.
//!   - [`RequestHandler`] — the injected dispatch of a carried [`HttpRequest`] to the node's *local*
//!     qfs (the SAME `crates/http`/`qfs_exec` path a local call runs). A bare
//!     `Fn(&HttpRequest) -> HttpResponse` is a handler.
//!   - [`run_resident`] — the resident node's service loop: drain inbound `Open` frames, dispatch
//!     each carried request, and reply `Data` + `Close` on the same `stream_id` (answering `Ping`
//!     with `Ping`). It opens NO inbound listener — it only reads/writes the already-open outbound
//!     connection, the "machines never open a port" guarantee.
//!   - [`collect_responses`] — the caller side: demux the `Data` frames back into a
//!     `stream_id → HttpResponse` map.
//!
//! Mirroring `qfs-google-auth`'s `HttpExchange`, the seam is synchronous and runtime-free so the
//! protocol is unit-tested without tokio; the async dial/listen loop stays in the binary leaf.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use qfs_http_core::{HttpRequest, HttpResponse};

use crate::frame::{decode_one, FrameBody, FrameKind, TunnelFrame};
use crate::TunnelError;

/// The synchronous, runtime-free seam over which whole [`TunnelFrame`]s are exchanged. The live
/// implementation wraps a tokio socket to the relay (a documented seam in the `qfs` binary); the
/// hermetic [`InMemoryDuplex`] implements it with no socket.
pub trait FrameTransport {
    /// Send one frame (append it to the outbound side).
    ///
    /// # Errors
    /// [`TunnelError`] if the underlying transport faults (e.g. a poisoned in-memory buffer).
    fn send(&self, frame: &TunnelFrame) -> Result<(), TunnelError>;

    /// Receive the next whole frame, or `Ok(None)` if none is currently available (the in-memory
    /// transport never blocks — a real socket implementation would await bytes).
    ///
    /// # Errors
    /// [`TunnelError`] if a buffered frame is structurally invalid or the transport faults.
    fn recv(&self) -> Result<Option<TunnelFrame>, TunnelError>;
}

/// The injected dispatch of a tunneled request to the node's *local* qfs. A bare closure is a
/// handler, so the binary wires it over the real `crates/http` handler path.
pub trait RequestHandler {
    /// Service one request locally and return the response (the SAME safety floor a local call
    /// runs: describe pure, preview side-effect-free, commit explicit + policy-gated).
    fn handle(&self, request: &HttpRequest) -> HttpResponse;
}

impl<F: Fn(&HttpRequest) -> HttpResponse> RequestHandler for F {
    fn handle(&self, request: &HttpRequest) -> HttpResponse {
        self(request)
    }
}

/// The resident node's service loop over one outbound connection. Drains every currently-available
/// inbound frame: an `Open` is dispatched through `handler` and answered with a `Data` (the
/// response) then a `Close` on the same `stream_id`; a `Ping` is answered with a `Ping`. Returns
/// the number of requests serviced.
///
/// It NEVER opens an inbound listener — it only reads and writes the already-open `transport`, which
/// is the outbound-only guarantee. `Data`/`Close` frames arriving inbound are ignored (the resident
/// node is the responder, not the caller).
///
/// # Errors
/// [`TunnelError`] if the transport faults while reading or writing.
pub fn run_resident<T: FrameTransport, H: RequestHandler>(
    transport: &T,
    handler: &H,
) -> Result<usize, TunnelError> {
    let mut serviced = 0_usize;
    while let Some(frame) = transport.recv()? {
        match frame.kind {
            FrameKind::Open => {
                if let FrameBody::Request(req) = &frame.body {
                    let response = handler.handle(req);
                    transport.send(&TunnelFrame::data(frame.stream_id, response))?;
                    transport.send(&TunnelFrame::close(frame.stream_id))?;
                    serviced += 1;
                }
            }
            FrameKind::Ping => transport.send(&TunnelFrame::ping())?,
            FrameKind::Data | FrameKind::Close => {}
        }
    }
    Ok(serviced)
}

/// The caller side: drain every currently-available inbound frame and demux the `Data` frames into
/// a `stream_id → HttpResponse` map (the multiplexing key sorts the responses for the caller). A
/// later `Data` on the same stream overwrites an earlier one (the last payload wins).
///
/// # Errors
/// [`TunnelError`] if the transport faults or a buffered frame is invalid.
pub fn collect_responses<T: FrameTransport>(
    transport: &T,
) -> Result<BTreeMap<u64, HttpResponse>, TunnelError> {
    let mut out = BTreeMap::new();
    while let Some(frame) = transport.recv()? {
        if let FrameBody::Response(resp) = frame.body {
            out.insert(frame.stream_id, resp);
        }
    }
    Ok(out)
}

/// A shared in-memory byte channel (one direction of the duplex).
type Pipe = Arc<Mutex<Vec<u8>>>;

/// An in-memory, **socket-free** [`FrameTransport`]: `send` encodes a frame onto its outbound pipe;
/// `recv` decodes one whole frame off its inbound pipe (draining exactly its bytes via the real
/// [`crate::frame`] codec). Two crossed ends are produced by [`duplex`], so a test wires a caller
/// end to a resident end and exercises the full encode → wire → decode path with no I/O.
pub struct InMemoryDuplex {
    send_to: Pipe,
    recv_from: Pipe,
}

impl InMemoryDuplex {
    fn lock(pipe: &Pipe) -> Result<std::sync::MutexGuard<'_, Vec<u8>>, TunnelError> {
        pipe.lock().map_err(|_| TunnelError::TransportPoisoned)
    }
}

impl FrameTransport for InMemoryDuplex {
    fn send(&self, frame: &TunnelFrame) -> Result<(), TunnelError> {
        Self::lock(&self.send_to)?.extend_from_slice(&frame.encode());
        Ok(())
    }

    fn recv(&self) -> Result<Option<TunnelFrame>, TunnelError> {
        let mut buf = Self::lock(&self.recv_from)?;
        match decode_one(&buf)? {
            Some((frame, consumed)) => {
                buf.drain(0..consumed);
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }
}

/// Create a connected pair of in-memory tunnel ends: bytes the first end sends are received by the
/// second and vice versa. The "caller/relay" side and the "resident node" side of a hermetic test.
#[must_use]
pub fn duplex() -> (InMemoryDuplex, InMemoryDuplex) {
    let a: Pipe = Arc::new(Mutex::new(Vec::new()));
    let b: Pipe = Arc::new(Mutex::new(Vec::new()));
    (
        InMemoryDuplex {
            send_to: a.clone(),
            recv_from: b.clone(),
        },
        InMemoryDuplex {
            send_to: b,
            recv_from: a,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_http_core::HttpMethod;

    /// A local handler that echoes the request's URL and method into a JSON-ish body, and surfaces
    /// the inbound `Authorization` header back as a (non-sensitive) marker header so the test can
    /// confirm the request crossed intact AND that the redaction property holds independently.
    fn echo_handler(req: &HttpRequest) -> HttpResponse {
        let body = format!("{} {}", req.method, req.url).into_bytes();
        let had_auth = req.header_value("authorization").is_some();
        HttpResponse::new(200, body).header("X-Had-Auth", if had_auth { "yes" } else { "no" })
    }

    #[test]
    fn request_response_multiplexing_over_an_in_memory_duplex() {
        let (caller, resident) = duplex();

        // The relay routes TWO inbound requests down the one connection, interleaved by stream_id.
        let req1 = HttpRequest::new(
            HttpMethod::Get,
            "https://node/hosts/acme-ci/claude/sessions",
        )
        .header("Authorization", "Bearer caller-token");
        let req2 = HttpRequest::new(
            HttpMethod::Post,
            "https://node/hosts/acme-ci/claude/instructions",
        )
        .header("Authorization", "Bearer caller-token")
        .with_body(b"rebase onto main".to_vec());
        caller
            .send(&TunnelFrame::open(1, req1))
            .expect("send open 1");
        caller
            .send(&TunnelFrame::open(2, req2))
            .expect("send open 2");

        // The resident node services both against its local qfs over the SAME connection.
        let serviced = run_resident(&resident, &echo_handler).expect("resident services frames");
        assert_eq!(serviced, 2, "both multiplexed requests are serviced");

        // The caller demuxes the responses back, keyed by stream.
        let responses = collect_responses(&caller).expect("collect responses");
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[&1].body,
            b"GET https://node/hosts/acme-ci/claude/sessions"
        );
        assert_eq!(
            responses[&2].body,
            b"POST https://node/hosts/acme-ci/claude/instructions"
        );
        // The bearer identity crossed the tunnel to the destination (where its auth still gates).
        assert_eq!(responses[&1].header_value("x-had-auth"), Some("yes"));
        assert_eq!(responses[&2].header_value("x-had-auth"), Some("yes"));
    }

    #[test]
    fn a_ping_is_answered_with_a_ping_keepalive() {
        let (caller, resident) = duplex();
        caller.send(&TunnelFrame::ping()).expect("send ping");
        let serviced = run_resident(&resident, &echo_handler).expect("resident handles ping");
        assert_eq!(serviced, 0, "a ping services no request");
        let mut saw_ping = false;
        while let Some(frame) = caller.recv().expect("recv") {
            if frame.kind == FrameKind::Ping {
                saw_ping = true;
            }
        }
        assert!(saw_ping, "the resident answers a Ping with a Ping");
    }

    #[test]
    fn the_tunnel_wire_never_carries_a_token_in_a_logged_debug() {
        // Belt-and-suspenders over the frame-level test: even the multiplexed Open frame a caller
        // puts on the wire redacts the bearer token when logged.
        let (caller, _resident) = duplex();
        let req = HttpRequest::new(HttpMethod::Get, "https://node/x")
            .header("Authorization", "Bearer wire-secret-token");
        let frame = TunnelFrame::open(1, req);
        caller.send(&frame).expect("send");
        assert!(!format!("{frame:?}").contains("wire-secret-token"));
    }
}
