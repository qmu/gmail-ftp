//! t80 (roadmap **M5** — decision U / §4.5): the **pure end-to-end (E2E) attendance gate** — the
//! fail-closed decision that makes a HIGH-SENSITIVITY (per-recipient-wrapped) connection unusable by
//! an autonomous agent acting **unattended**, because such a connection's data-key is recoverable
//! only by a human recipient holding their private key.
//!
//! This module is the *decision*, not the *I/O* nor the *crypto* — the same shape as t54's
//! [`crate::bind_gate`] and t81's [`crate::shared_use_gate`]. The per-recipient wrap PRIMITIVE lives
//! in `qfs-oauth` (it needs `p256` ECDH, which must not enter this wasm-buildable leaf); the
//! per-recipient wrapped-DEK STORE lives binary-side. This leaf holds only the pure, secret-free
//! predicate the binary wires real state into:
//!
//! - **is_e2e** — does the connection carry per-recipient wraps (a high-sensitivity / E2E
//!   connection)? The binary reads this from the project DB (a connection with any
//!   `e2e_recipient_wrap` row is E2E).
//! - **attended** — is a human in the loop for this commit (an interactive CLI run), as opposed to an
//!   autonomous agent over MCP / a server-fire / a one-shot with no TTY? The binary derives this from
//!   the `RunMode`.
//!
//! ## Why this is the explicit trade-off (decision U / J)
//! E2E buys server-compromise resistance (§4.5 threat 3): the server storing the ciphertext cannot
//! by itself decrypt the secret. The unavoidable cost is that an UNATTENDED agent — which has no
//! private key — cannot use such a connection either. This gate makes that trade-off explicit and
//! auditable rather than silently failing deep in a decrypt: an E2E connection used unattended is
//! refused **pending a human unwrap**, fail-closed, BEFORE any per-recipient DEK is touched.
//!
//! ## Why pure (the §3 purity invariant)
//! A `Plan` carries a connection *selector*, never the secret. The attendance decision happens at
//! bind/commit time, downstream of `describe`/`preview`, and BEFORE the DEK is unwrapped:
//! [`e2e_attendance_gate`] returning `Err` means the bind resolver never recovers the per-recipient
//! DEK for an unattended actor. Keeping the decision pure (no DB, no crypto, no [`Secret`](crate::Secret))
//! lets the binary unit-test it hermetically and keeps `qfs-secrets` wasm-buildable.

/// Why USE of a high-sensitivity (E2E) connection was refused — structured and **secret-free** (a
/// connection name is metadata, never a credential). AI-actionable: it names exactly the missing
/// precondition (a human in the loop to unwrap with their key) so the operator/agent knows the
/// remedy. Mirrors [`crate::SharedUseError`] / [`crate::ConsentError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum E2eUseError {
    /// The connection is end-to-end (its data-key is wrapped only to members' public keys), but the
    /// commit is running **unattended** (an autonomous agent / server-fire with no human key in the
    /// loop). **Fail closed** (decision U / §4.5): the per-recipient DEK is never recovered for an
    /// unattended actor — the connection cannot be used without a human recipient unwrap. Actionable:
    /// a member who holds a recipient key must run the commit interactively (attended).
    #[error(
        "connection '{connection}' is end-to-end (high-sensitivity) and cannot be used by an \
         autonomous agent unattended — a human recipient must unwrap it with their key in the loop"
    )]
    UnattendedRefused {
        /// The connection name that was refused (metadata, never a secret).
        connection: String,
    },
}

impl E2eUseError {
    /// A short, stable error code for structured/JSON surfaces and AI feedback (mirrors
    /// [`crate::SharedUseError::code`]).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            E2eUseError::UnattendedRefused { .. } => "e2e_connection_unattended",
        }
    }
}

/// The bind-time **E2E attendance gate**: decide whether a commit may USE a connection given whether
/// it is end-to-end (`is_e2e`) and whether a human is in the loop (`attended`).
///
/// - A **non-E2E** connection (`is_e2e == false`) is never gated by this mechanism — `Ok(())`
///   regardless of `attended` (the server can unwrap it as before; the existing managed-tier path is
///   unaffected).
/// - An **E2E** connection (`is_e2e == true`) **fails closed** when run UNATTENDED
///   ([`E2eUseError::UnattendedRefused`]); it is allowed only when `attended` (a human recipient is
///   present to unwrap with their private key).
///
/// Pure and secret-free: it reasons over two booleans the binary derives (the connection's E2E bit
/// from the project DB, the attendance from the `RunMode`); it never sees a token. `connection` is
/// carried only to name it in the refusal.
///
/// # Errors
/// [`E2eUseError::UnattendedRefused`] when an E2E connection is used unattended.
pub fn e2e_attendance_gate(
    is_e2e: bool,
    connection: &str,
    attended: bool,
) -> Result<(), E2eUseError> {
    if !is_e2e {
        // Not high-sensitivity: ungated. The server-unwrappable managed path is unchanged.
        return Ok(());
    }
    if !attended {
        return Err(E2eUseError::UnattendedRefused {
            connection: connection.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_e2e_connection_is_never_gated() {
        // is_e2e == false ⇒ Ok regardless of attendance — the managed server-unwrappable path.
        assert!(e2e_attendance_gate(false, "work", false).is_ok());
        assert!(e2e_attendance_gate(false, "work", true).is_ok());
    }

    #[test]
    fn an_e2e_connection_unattended_is_refused_closed() {
        let err = e2e_attendance_gate(true, "secrets-vault", false).unwrap_err();
        assert_eq!(err.code(), "e2e_connection_unattended");
        // Actionable + names the connection.
        assert!(err.to_string().contains("secrets-vault"));
    }

    #[test]
    fn an_e2e_connection_attended_proceeds() {
        // A human in the loop (interactive CLI) may use it — they hold a recipient key to unwrap.
        assert!(e2e_attendance_gate(true, "secrets-vault", true).is_ok());
    }

    #[test]
    fn the_refusal_never_carries_a_secret_marker() {
        let err = e2e_attendance_gate(true, "vault", false).unwrap_err();
        let rendered = format!("{err:?} {err}");
        for forbidden in ["token", "secret", "password", "ciphertext", "private key"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "E2E refusal must be secret-free: leaked `{forbidden}`"
            );
        }
    }
}
