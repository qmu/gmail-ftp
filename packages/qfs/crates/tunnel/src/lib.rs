//! `qfs-tunnel` — the **agent-fabric transport PROTOCOL CORE** (t63, roadmap M7 — decisions L + N,
//! part 3.3 "the fleet"): the pure, tokio-free leaf that owns the qfs-native outbound tunnel wire
//! format and the relay registration handshake.
//!
//! ## What the tunnel is (the cross-machine shape, RFD §3.3)
//! A self-hosted `qfs` behind NAT/firewall runs a **resident node**. Rather than opening an inbound
//! port (which a home laptop or an office desktop cannot safely do), the resident node dials an
//! **OUTBOUND** connection UP to a qfs Cloud **relay** and registers. A cloud-side caller (another
//! teammate's agent) then addresses that node by name; the relay routes the caller's request DOWN
//! the already-open outbound connection, the resident node services it against its *local* qfs, and
//! the response rides back up the same connection. The machine **never opens an inbound port** — the
//! "outbound-only" security property (decision L). Using the tunnel **requires a qfs Cloud sign-in**
//! (decision N), enforced at the relay handshake.
//!
//! ## What lives HERE (hermetic) vs. what is a DOCUMENTED SEAM
//! A live outbound TCP/TLS dial to a running qfs Cloud relay is network/native and **cannot** be
//! tested hermetically, so this crate is *only the testable core*, all exercised over an in-memory
//! transport with NO socket:
//!   1. [`frame`] — the length-prefixed [`TunnelFrame`]/[`FrameKind`] codec (encode/decode
//!      round-trip), carrying [`qfs_http_core::HttpRequest`]/[`HttpResponse`] so the **single
//!      redaction authority** is inherited (a frame's `Debug` redacts a bearer token automatically).
//!   2. [`handshake`] — the [`RelayHello`]/[`RelayAccepted`] registration messages and
//!      [`relay_accept`], which validates the `cloud_token` through an **injected**
//!      [`TokenValidator`] seam (decision N — a bad/absent token is *refused*; the tunnel adds no
//!      second auth model, it reuses the t50 bearer identity passed in as a closure/trait).
//!   3. [`mux`] — request/response **multiplexing** over an in-memory duplex ([`FrameTransport`] +
//!      [`run_resident`]): many concurrent `stream_id`s interleave on one connection.
//!
//! The live dial/listen loop (the tokio half: connect to the relay, frame the bytes over a real
//! socket, reconnect/backoff) is a **documented seam** wired by the `qfs` binary (`src/tunnel.rs`),
//! where tokio dead-ends in the terminal leaf. This crate stays **pure** so the protocol is
//! unit-tested without a runtime.
//!
//! ## The safety floor is inherited, not re-invented
//! A request arriving over the tunnel is still a qfs statement serviced by the **same**
//! `crates/http` + `qfs_exec` path a local call runs (describe is pure, preview touches nothing,
//! commit is explicit, irreversible needs the extra ack). The relay is **untrusted transport**: it
//! routes opaque frames and cannot read a secret — the bearer/MCP auth in front of the binding and
//! the default-deny `POLICY` gate at the destination still bound every cross-machine call. The
//! tunnel widens *reach*, never *authorization* (one-engine-three-faces). Redaction is the
//! [`qfs_http_core`] authority: a frame's `Debug` never renders a sensitive header value, and the
//! handshake's `cloud_token` is redacted in its `Debug`.

// Test modules assert/expect/unwrap freely; the strict workspace lint is relaxed under cfg(test).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod frame;
pub mod handshake;
pub mod mux;

pub use frame::{decode, decode_one, FrameBody, FrameKind, TunnelFrame, FRAME_VERSION};
pub use handshake::{
    relay_accept, HandshakeError, RelayAccepted, RelayHello, TokenValidator, MAX_NODE_ID_LEN,
};
pub use mux::{
    collect_responses, duplex, run_resident, FrameTransport, InMemoryDuplex, RequestHandler,
};

/// The error type for the tunnel protocol core — every variant is **secret-free** (built from the
/// frame *shape*, never a header value or a token). Decode distinguishes a *malformed* frame (a hard
/// error) from a merely *incomplete* one ([`decode_one`] returns `Ok(None)` when more bytes are
/// needed), so a streaming reader can wait for the rest without treating a partial read as a fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TunnelError {
    /// The frame declared a version this build does not understand (forward-compat fence).
    #[error("unsupported tunnel frame version {0} (this build speaks v{FRAME_VERSION})")]
    UnsupportedVersion(u8),
    /// The frame's kind tag is not one of [`FrameKind`]'s known variants.
    #[error("unknown tunnel frame kind tag {0}")]
    UnknownKind(u8),
    /// A complete-frame [`decode`] was handed fewer bytes than one whole frame needs.
    #[error("incomplete tunnel frame: more bytes are needed to decode one frame")]
    Incomplete,
    /// The frame's bytes are structurally invalid (bad length prefix, non-UTF-8 string, an
    /// unknown HTTP method tag, a non-empty Close/Ping body, trailing bytes). The `&'static str`
    /// names the structural fault and carries **no** payload bytes.
    #[error("malformed tunnel frame: {0}")]
    Malformed(&'static str),
    /// An in-memory transport's shared buffer lock was poisoned by a panicking peer. Surfaced
    /// (rather than re-panicking) so a `dyn`-driven loop fails closed.
    #[error("tunnel transport buffer lock poisoned")]
    TransportPoisoned,
}
