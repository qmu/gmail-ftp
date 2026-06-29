//! The relay **registration handshake** — the authentication gate of the tunnel (decision N).
//!
//! Before any traffic flows, the resident node sends a [`RelayHello`] up the freshly-dialed
//! outbound connection: its `node_id` (how a caller will address it, e.g. `acme-ci`) and a
//! `cloud_token` proving a **qfs Cloud sign-in**. The relay calls [`relay_accept`], which validates
//! the token through an **injected** [`TokenValidator`] — the SAME t50 bearer identity the MCP
//! binding already gates with, passed in as a closure/trait so the tunnel adds **no second auth
//! model**. A missing, empty, or invalid token is *refused* with a secret-free [`HandshakeError`];
//! only on success does the relay mint a [`RelayAccepted`] session and begin routing frames.
//!
//! The relay is **untrusted transport**: accepting the handshake authorizes the node to *register*,
//! it does not bypass anything downstream. Every inbound request the relay later routes is STILL
//! gated by the destination node's bearer/MCP auth and its default-deny `POLICY` — the handshake
//! gates *reach onto the fabric*, never *authorization of an effect*.
//!
//! ## Secret discipline
//! [`RelayHello`] carries the live `cloud_token`, so its `Debug` is **manual** and redacts the token
//! (emitting [`qfs_secrets::REDACTED`]) — a hello is never logged with the sign-in token in
//! cleartext, the same discipline [`qfs_http_core`] applies to sensitive headers. The token is still
//! present for the wire send (serde), exactly as a bearer header value is present on an
//! `HttpRequest` it is redacted from in `Debug`.

use core::fmt;

use serde::{Deserialize, Serialize};

/// An upper bound on a `node_id`'s length — a hello with a longer id is refused, so a hostile or
/// buggy peer cannot wedge an unbounded string into the relay's `node_id → session` table.
pub const MAX_NODE_ID_LEN: usize = 253;

/// The resident node's registration message: who it is and its proof of a qfs Cloud sign-in.
///
/// `Debug` is **manual** and redacts `cloud_token`; `Serialize`/`Deserialize` carry the token for
/// the wire (the two are orthogonal — redaction is for logs, serialization is for the connection).
#[derive(Clone, Serialize, Deserialize)]
pub struct RelayHello {
    /// How a caller addresses this node on the fabric (e.g. `acme-ci`). Bounded by
    /// [`MAX_NODE_ID_LEN`] and required non-empty.
    pub node_id: String,
    /// The qfs Cloud sign-in token (a t50 bearer access token). Validated by the injected
    /// [`TokenValidator`]; **redacted** in `Debug`.
    pub cloud_token: String,
}

impl RelayHello {
    /// Construct a hello.
    #[must_use]
    pub fn new(node_id: impl Into<String>, cloud_token: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            cloud_token: cloud_token.into(),
        }
    }

    /// Serialize to the wire bytes the resident node sends first on a freshly-dialed connection
    /// (serde JSON — the "serde" half the frame codec pairs with).
    ///
    /// # Errors
    /// [`serde_json::Error`] only on an allocator failure (the shape is always serializable).
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Parse a hello the relay received.
    ///
    /// # Errors
    /// [`serde_json::Error`] if the bytes are not a well-formed hello.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Manual, **redacting** `Debug`: the `node_id` is shown (it is public addressing), the `cloud_token`
/// never is — so a hello dumped into a log line cannot leak the sign-in token.
impl fmt::Debug for RelayHello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayHello")
            .field("node_id", &self.node_id)
            .field("cloud_token", &qfs_secrets::REDACTED)
            .finish()
    }
}

/// The relay's success reply: the routing session the node is now registered under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayAccepted {
    /// The session id the relay keys this node's outbound connection by (the routing handle for
    /// inbound frames addressed to the node).
    pub session_id: String,
}

impl RelayAccepted {
    /// Construct an acceptance.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

/// Why a [`relay_accept`] refused a hello — every variant is **secret-free** (it names the failure
/// class, never the token).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HandshakeError {
    /// The hello carried no (or a blank) `cloud_token` — using the tunnel REQUIRES a qfs Cloud
    /// sign-in (decision N).
    #[error("relay handshake refused: a qfs Cloud sign-in token is required")]
    MissingToken,
    /// The `cloud_token` was present but the injected validator rejected it (bad signature /
    /// wrong audience / expired). The relay never sees *why* in detail — only that it is invalid.
    #[error("relay handshake refused: invalid or expired qfs Cloud sign-in token")]
    InvalidToken,
    /// The `node_id` was empty.
    #[error("relay handshake refused: a non-empty node_id is required")]
    EmptyNodeId,
    /// The `node_id` exceeded [`MAX_NODE_ID_LEN`].
    #[error("relay handshake refused: node_id exceeds {MAX_NODE_ID_LEN} bytes")]
    NodeIdTooLong,
}

/// The injected token-validation seam (decision N). The relay does **not** know how to verify a
/// qfs Cloud sign-in token itself — it is handed a validator that wraps the SAME t50 bearer
/// validation the MCP binding uses, so there is exactly one identity point. Any
/// `Fn(&str) -> bool` is a validator, so a caller wires it from a closure over
/// `qfs_oauth::verify_access_token`.
pub trait TokenValidator {
    /// Whether `token` is a currently-valid qfs Cloud sign-in.
    fn validate(&self, token: &str) -> bool;
}

impl<F: Fn(&str) -> bool> TokenValidator for F {
    fn validate(&self, token: &str) -> bool {
        self(token)
    }
}

/// Validate a [`RelayHello`] and, on success, mint the [`RelayAccepted`] the node registers under.
/// This is the ONE gate that enforces decision N: a missing/empty/invalid token is refused, an
/// over-long or empty `node_id` is refused, and only a hello with a validator-accepted token is
/// admitted onto the fabric.
///
/// `session_id` is supplied by the caller (the relay owns session-id minting — a random,
/// non-guessable handle in production; injected here so the core stays deterministic/testable).
///
/// # Errors
/// A [`HandshakeError`] naming the refusal class (secret-free).
pub fn relay_accept<V: TokenValidator>(
    hello: &RelayHello,
    validator: &V,
    session_id: impl Into<String>,
) -> Result<RelayAccepted, HandshakeError> {
    if hello.node_id.is_empty() {
        return Err(HandshakeError::EmptyNodeId);
    }
    if hello.node_id.len() > MAX_NODE_ID_LEN {
        return Err(HandshakeError::NodeIdTooLong);
    }
    if hello.cloud_token.trim().is_empty() {
        return Err(HandshakeError::MissingToken);
    }
    if !validator.validate(&hello.cloud_token) {
        return Err(HandshakeError::InvalidToken);
    }
    Ok(RelayAccepted::new(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A validator that accepts exactly one good token — stands in for the injected t50 bearer
    /// validation (`qfs_oauth::verify_access_token`).
    fn only_good(token: &str) -> bool {
        token == "good-cloud-token"
    }

    #[test]
    fn a_valid_hello_is_accepted_and_yields_the_session() {
        let hello = RelayHello::new("acme-ci", "good-cloud-token");
        let accepted = relay_accept(&hello, &only_good, "sess-1").expect("valid hello accepted");
        assert_eq!(accepted, RelayAccepted::new("sess-1"));
    }

    #[test]
    fn a_hello_with_no_token_is_refused() {
        let hello = RelayHello::new("acme-ci", "");
        assert_eq!(
            relay_accept(&hello, &only_good, "sess-1"),
            Err(HandshakeError::MissingToken)
        );
    }

    #[test]
    fn a_blank_token_is_refused() {
        let hello = RelayHello::new("acme-ci", "   ");
        assert_eq!(
            relay_accept(&hello, &only_good, "sess-1"),
            Err(HandshakeError::MissingToken)
        );
    }

    #[test]
    fn an_invalid_token_is_refused() {
        let hello = RelayHello::new("acme-ci", "forged-token");
        assert_eq!(
            relay_accept(&hello, &only_good, "sess-1"),
            Err(HandshakeError::InvalidToken)
        );
    }

    #[test]
    fn an_empty_node_id_is_refused() {
        let hello = RelayHello::new("", "good-cloud-token");
        assert_eq!(
            relay_accept(&hello, &only_good, "sess-1"),
            Err(HandshakeError::EmptyNodeId)
        );
    }

    #[test]
    fn an_over_long_node_id_is_refused() {
        let hello = RelayHello::new("n".repeat(MAX_NODE_ID_LEN + 1), "good-cloud-token");
        assert_eq!(
            relay_accept(&hello, &only_good, "sess-1"),
            Err(HandshakeError::NodeIdTooLong)
        );
    }

    #[test]
    fn a_hello_round_trips_through_serde() {
        let hello = RelayHello::new("acme-ci", "good-cloud-token");
        let bytes = hello.encode().expect("encode hello");
        let back = RelayHello::decode(&bytes).expect("decode hello");
        assert_eq!(back.node_id, "acme-ci");
        assert_eq!(back.cloud_token, "good-cloud-token");
    }

    #[test]
    fn a_hello_debug_never_leaks_the_cloud_token() {
        let hello = RelayHello::new("acme-ci", "super-secret-sign-in");
        let dbg = format!("{hello:?}");
        assert!(
            !dbg.contains("super-secret-sign-in"),
            "the cloud_token must never appear in a hello's Debug: {dbg}"
        );
        assert!(
            dbg.contains("acme-ci"),
            "the node_id is public addressing: {dbg}"
        );
        assert!(
            dbg.contains(qfs_secrets::REDACTED),
            "redaction marker present: {dbg}"
        );
    }
}
