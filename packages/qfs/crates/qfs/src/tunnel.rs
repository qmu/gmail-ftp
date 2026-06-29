//! `qfs serve` agent-fabric transport composition root (t63, roadmap M7 — decisions L + N).
//!
//! This module is the **binary leaf** where the qfs-native outbound tunnel + relay is wired. The
//! protocol CORE — the frame codec, the relay registration handshake, and the request/response
//! multiplexing — lives in the pure, tokio-free [`qfs_tunnel`] crate and is unit-tested there over
//! an in-memory transport. What lives HERE is the composition: adapting the t50 bearer identity into
//! the handshake's injected token validator, dispatching an inbound frame's carried request to the
//! local `crates/http` handler path, and (the **documented seam**) opening the live outbound
//! TCP/TLS connection to a qfs Cloud relay.
//!
//! ## What is wired vs. what is a DOCUMENTED SEAM (honesty-first)
//! A real outbound connection to a live qfs Cloud relay is network/native and **cannot** be tested
//! hermetically, so it is deliberately NOT implemented here yet — there is no live tunnel and this
//! ticket does not claim one. The pieces that ARE real and hermetic:
//!   - [`cloud_token_validator`] adapts the SAME t50 bearer validation the MCP binding gates with
//!     into the handshake's [`qfs_tunnel::TokenValidator`] seam (decision N — using the tunnel
//!     requires a qfs Cloud sign-in), so there is exactly one identity point.
//!   - [`accept_registration`] is the relay-side gate the live accept loop would call per inbound
//!     node connection (it delegates to [`qfs_tunnel::relay_accept`]).
//!
//! The pieces that are a **seam** (a future ticket lands the live dial — flagged in the t63 PR as an
//! open product decision, NOT guessed here):
//!   - the outbound TCP/TLS **dial** to the relay (the resident node connecting UP),
//!   - the relay's own **hosting** (a long-lived process vs. Cloudflare Workers) and its
//!     **addressing/discovery** model (how a caller names `acme-ci`),
//!   - **reconnect/backoff** semantics on a relay restart.
//!
//! ## The security model is unchanged (the tunnel is transport, not a trust boundary)
//! The relay is **untrusted transport**: it routes opaque frames and cannot read a secret. A request
//! arriving over the tunnel is dispatched through the SAME `crates/http` + `qfs_exec` path a local
//! call runs, so the bearer/MCP auth in front of the binding and the default-deny `POLICY` gate at
//! the destination still bound every cross-machine call. The tunnel adds *reach*, never
//! *authorization*. Redaction is inherited from [`qfs_http_core`]: a frame's `Debug` never renders a
//! sensitive header value, and the handshake's `cloud_token` is redacted in its `Debug`.

use qfs_tunnel::{relay_accept, HandshakeError, RelayAccepted, RelayHello, TokenValidator};

/// Adapt an owned bearer-validation closure into a [`TokenValidator`] for the relay handshake.
///
/// The `qfs` binary builds `validate` as a closure over the SAME t50 access-token verification the
/// MCP `BearerAuthorizer` uses (signature + `iss`/`aud`/`exp` against the AS's JWKS via
/// `qfs_oauth::verify_access_token`). Wrapping it here — rather than teaching `qfs-tunnel` how to
/// verify a token — keeps the protocol core pure and pins decision N to the one identity point: a
/// hello whose `cloud_token` the closure rejects is refused at [`accept_registration`].
pub struct CloudTokenValidator {
    validate: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl CloudTokenValidator {
    /// Wrap the binary's bearer-validation closure.
    #[must_use]
    pub fn new(validate: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            validate: Box::new(validate),
        }
    }
}

impl TokenValidator for CloudTokenValidator {
    fn validate(&self, token: &str) -> bool {
        (self.validate)(token)
    }
}

/// The relay-side registration gate: validate a resident node's [`RelayHello`] against the qfs Cloud
/// sign-in (decision N) and, on success, mint the routing session it registers under. This is the
/// call the live accept loop (the documented seam) makes per inbound node connection, factored out
/// so the gate itself is hermetic.
///
/// # Errors
/// A [`HandshakeError`] (secret-free) if the token is missing/invalid or the `node_id` is malformed.
pub fn accept_registration(
    hello: &RelayHello,
    validator: &CloudTokenValidator,
    session_id: impl Into<String>,
) -> Result<RelayAccepted, HandshakeError> {
    relay_accept(hello, validator, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_validator_gates_registration_on_the_cloud_sign_in() {
        // Stand in for the t50 bearer validation the binary actually injects.
        let validator = CloudTokenValidator::new(|t: &str| t == "valid-cloud-signin");

        let good = RelayHello::new("acme-ci", "valid-cloud-signin");
        assert!(accept_registration(&good, &validator, "sess-1").is_ok());

        let forged = RelayHello::new("acme-ci", "forged");
        assert_eq!(
            accept_registration(&forged, &validator, "sess-1"),
            Err(HandshakeError::InvalidToken)
        );

        let absent = RelayHello::new("acme-ci", "");
        assert_eq!(
            accept_registration(&absent, &validator, "sess-1"),
            Err(HandshakeError::MissingToken)
        );
    }
}
