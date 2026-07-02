//! t81 (roadmap **M5** — decision U / §3.3): the **pure shared-connection USE gate** — the
//! fail-closed decision that makes a project/team-owned connection unusable for a member whose
//! actor-policy does not grant the connection's scope.
//!
//! This module is the *decision*, not the *I/O* — the same shape as t54's [`crate::bind_gate`]. It
//! holds one pure, secret-free predicate the binary's commit-time bind wires the real state into:
//!
//! - **owner** — is the connection owned by the user ([`OwnerScope::Me`]) or by the project
//!   ([`OwnerScope::Project`])? The binary reads this from the project DB's `shared_connection`
//!   table (a project-owned connection has a row there).
//! - **actor_granted** — has the acting member's t57 actor-policy granted them the connection's
//!   scope? The binary computes this in `crates/qfs` by evaluating the project policy against the
//!   resolved actor over the connection's realm scope (`qfs_server::evaluate_shared_use`) — the
//!   policy actor model stays in `qfs-server`, this leaf only consumes the resulting boolean.
//!
//! ## Why pure (the §3 purity invariant)
//! A `Plan` carries a connection *selector*, never the secret. The owner/grant decision happens at
//! **bind/commit time**, downstream of `describe`/`preview`, and BEFORE the secret is decrypted:
//! [`shared_use_gate`] returning `Err` means the bind resolver never calls
//! [`Secrets::get`](crate::Secrets::get), so the DEK/secret is never unwrapped for an unauthorized
//! actor (default-deny). Keeping the decision pure (no DB, no network, no [`Secret`](crate::Secret))
//! lets the binary unit-test the gate hermetically and keeps `qfs-secrets` wasm-buildable — nothing
//! here touches the (native-only) envelope or any I/O.
//!
//! ## What it does NOT change
//! This gate decides **who may invoke a bind**, never the at-rest crypto: the credential stays
//! envelope-encrypted exactly as before (t43), and a user-owned connection ([`OwnerScope::Me`]) is
//! entirely unaffected — it short-circuits to `Ok`, so the team-sharing model adds zero gating to
//! the existing single-operator path.

use crate::key::OwnerScope;

/// Why USE of a project/team-owned connection was refused — structured and **secret-free** (a
/// connection name and a scope are metadata, never a credential). AI-actionable: the message names
/// exactly what is missing (a policy grant for the connection's scope) so the operator knows the
/// remedy. Mirrors [`crate::ConsentError`] / [`crate::ScopeError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SharedUseError {
    /// The connection is project/team-owned, but the acting member's actor-policy does not grant
    /// them the connection's scope. **Fail closed** (decision U / §3.3): a member with no grant
    /// cannot use a shared connection, and the secret is NEVER decrypted for them. Actionable: a
    /// project admin must grant the member's actor the connection's scope (a t57 `ALLOW … AT
    /// /projects/<proj>/…` policy) before they can use it.
    #[error(
        "shared connection '{connection}' is project-owned and your actor is not granted its \
         scope '{scope}' — a project admin must grant your actor that scope before you can use it"
    )]
    NotGranted {
        /// The connection name that was refused (metadata, never a secret).
        connection: String,
        /// The connection's required scope the actor lacks (a realm path glob — metadata, never a
        /// secret).
        scope: String,
    },
}

impl SharedUseError {
    /// A short, stable error code for structured/JSON surfaces and AI feedback (mirrors
    /// [`crate::ConsentError::code`]).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            SharedUseError::NotGranted { .. } => "shared_connection_not_granted",
        }
    }
}

/// The bind-time **shared-connection USE gate**: decide whether the acting member may USE a
/// connection of ownership `owner`, given whether their actor-policy `actor_granted` the
/// connection's scope.
///
/// - A **user-owned** connection ([`OwnerScope::Me`]) is never gated by this mechanism — `Ok(())`
///   regardless of `actor_granted` (the team-sharing rule is about project-owned connections only;
///   the existing single-operator path is unaffected).
/// - A **project-owned** connection ([`OwnerScope::Project`]) **fails closed**: it requires the
///   member's actor-policy to have granted the connection's scope ([`SharedUseError::NotGranted`]
///   when `!actor_granted`). The grant is the t57 actor-policy decision the binary computes BEFORE
///   this gate — and only an `Ok` here lets the bind resolve the secret.
///
/// Pure and secret-free: it reasons over the ownership label and one boolean the binary derives from
/// the project policy + resolved actor; it never sees a token. `connection` / `scope` are carried
/// only to name them in the [`SharedUseError::NotGranted`] message.
///
/// # Errors
/// [`SharedUseError::NotGranted`] when a project-owned connection's actor is not granted its scope.
pub fn shared_use_gate(
    owner: OwnerScope,
    connection: &str,
    scope: &str,
    actor_granted: bool,
) -> Result<(), SharedUseError> {
    if !owner.is_project() {
        // User-owned: ungated. Team-sharing is a project-owned concern only.
        return Ok(());
    }
    if !actor_granted {
        return Err(SharedUseError::NotGranted {
            connection: connection.to_string(),
            scope: scope.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_owned_connection_is_never_gated() {
        // Me ⇒ Ok regardless of the (irrelevant) grant boolean — the single-operator path.
        assert!(shared_use_gate(OwnerScope::Me, "work", "/me/mail/**", false).is_ok());
        assert!(shared_use_gate(OwnerScope::Me, "work", "/me/mail/**", true).is_ok());
    }

    #[test]
    fn a_project_owned_connection_without_a_grant_is_refused_closed() {
        let err = shared_use_gate(OwnerScope::Project, "team-gh", "/projects/acme/**", false)
            .unwrap_err();
        assert_eq!(err.code(), "shared_connection_not_granted");
        // Secret-free + actionable: names the connection + the missing scope, no token.
        assert!(err.to_string().contains("team-gh"));
        assert!(err.to_string().contains("/projects/acme/**"));
    }

    #[test]
    fn a_project_owned_connection_with_a_grant_proceeds() {
        assert!(shared_use_gate(OwnerScope::Project, "team-gh", "/projects/acme/**", true).is_ok());
    }

    #[test]
    fn the_refusal_never_carries_a_secret_marker() {
        // The error is built only from the connection name + scope — assert no secret words leak.
        let err =
            shared_use_gate(OwnerScope::Project, "team", "/projects/acme/**", false).unwrap_err();
        let rendered = format!("{err:?} {err}");
        for forbidden in ["token", "secret", "password", "ciphertext"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "shared-use refusal must be secret-free: leaked `{forbidden}`"
            );
        }
    }
}
