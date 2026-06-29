//! The [`Secrets`] trait — the **one** surface every driver and the server call to fetch
//! a credential (RFD-0001 §5/§8/§10) — its structured [`SecretError`], and the
//! scope-grant tie-in ([`ScopeError`], [`grant_scopes`]) that lets a driver request a
//! credential with required scopes and get a secret-free grant/deny.
//!
//! The trait is consumer-side and owned-DTO only: it trades in [`CredentialKey`],
//! [`ConnectionRecord`], and [`Secret`] — no vendor SDK type crosses it (§9). Every error
//! variant is secret-free by construction (the only one that *could* carry text,
//! [`SecretError::Backend`], takes a backend *description*, never the credential — see
//! the redaction test in `lib.rs`).

use crate::key::{ConnectionRecord, CredentialKey, DriverId};
use crate::secret::Secret;

/// The single secrets surface. Backends ([`crate::InMemoryStore`], [`crate::EnvStore`],
/// [`crate::LocalStore`], the wasm `WorkerStore`) implement it; drivers and the server
/// depend only on this trait (`&dyn Secrets`), never on a concrete backend.
///
/// `Send + Sync` so an `Arc<dyn Secrets>` can be threaded through the engine and shared
/// across the driver-bind context.
pub trait Secrets: Send + Sync {
    /// Fetch the credential for `key`.
    ///
    /// # Errors
    /// - [`SecretError::NotFound`] if no credential is stored for the key.
    /// - [`SecretError::Locked`] if the backend is sealed (e.g. no key/passphrase yet).
    /// - [`SecretError::Backend`] for any other backend failure (I/O, decode) — the
    ///   message describes the *backend operation*, never the credential.
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError>;

    /// Store `value` under `key`, replacing any existing credential. Takes the `Secret`
    /// by value (it is consumed into the store; no copy lingers in the caller).
    ///
    /// # Errors
    /// [`SecretError::Locked`] or [`SecretError::Backend`] on failure.
    fn put(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError>;

    /// Remove the credential for `key`. Removing an absent key is **not** an error
    /// (idempotent — `qfs connection remove` is replayable).
    ///
    /// # Errors
    /// [`SecretError::Locked`] or [`SecretError::Backend`] on failure.
    fn remove(&self, key: &CredentialKey) -> Result<(), SecretError>;

    /// List the stored connections, optionally filtered to one `driver`. Returns
    /// secret-free [`ConnectionRecord`]s (selectors + metadata only).
    ///
    /// # Errors
    /// [`SecretError::Locked`] or [`SecretError::Backend`] on failure.
    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<ConnectionRecord>, SecretError>;
}

/// A structured, **secret-free** store error (RFD §10 — secrets never enter error text).
///
/// `NotFound` carries the *key* (driver + connection — selectors, not the value). `Backend`
/// carries a description of the failing *operation*, never the credential; constructing
/// it from a `Secret` is impossible because `Secret` has no `Display`/`Into<String>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// No credential stored for this `(driver, connection)`. Actionable: run `qfs connection
    /// add` for the named connection.
    #[error("no credential for {}/{}", .0.driver.as_str(), .0.connection.as_str())]
    NotFound(CredentialKey),

    /// The backend is sealed and cannot answer (no decryption key / passphrase supplied,
    /// or the OS keyring is locked). Carries no key material.
    #[error("secret store is locked (no decryption key available)")]
    Locked,

    /// The credential for this `(driver, connection)` was **revoked** (t79 — offboarding /
    /// compromise). The connection is marked revoked and the bind path REFUSES to resolve it: the
    /// secret is NEVER decrypted or returned (default-deny). Carries the *key* (selectors), never
    /// the value. Actionable: re-mint it with `qfs connection rotate <driver> <connection>`, which
    /// replaces the secret and clears the revocation.
    #[error("credential for {}/{} is revoked", .0.driver.as_str(), .0.connection.as_str())]
    Revoked(CredentialKey),

    /// A backend operation failed. The string describes the *operation* (e.g. "reading
    /// credential blob", "permission check"), **never** the credential value.
    #[error("secret backend error: {0}")]
    Backend(String),
}

impl SecretError {
    /// A short, stable error code for structured/JSON surfaces and AI feedback.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SecretError::NotFound(_) => "secret_not_found",
            SecretError::Locked => "secret_locked",
            SecretError::Revoked(_) => "secret_revoked",
            SecretError::Backend(_) => "secret_backend",
        }
    }
}

/// The outcome of a scope check: which of a procedure's `requires_scopes` (the t13 hints
/// on [`qfs_driver::ProcSig`], passed in as owned labels) the resolved connection is granted.
///
/// This is the capability/scope tie-in: a driver requests a credential *with* required
/// scopes; [`grant_scopes`] compares them against the scopes the stored connection was
/// provisioned with and returns a structured grant or a secret-free [`ScopeError`]
/// listing exactly what is missing — AI-actionable, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeGrant {
    /// The scopes that were both requested and held — the granted intersection.
    pub granted: Vec<String>,
}

/// A scope was requested that the resolved connection does not hold. Secret-free: it lists
/// scope *labels* (the §10 hints), never a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("connection lacks required scope(s): {}", .missing.join(", "))]
pub struct ScopeError {
    /// The requested scopes the connection does not hold — what to re-consent for.
    pub missing: Vec<String>,
    /// The scopes the connection *does* hold, for context (never a credential).
    pub held: Vec<String>,
}

impl ScopeError {
    /// A short, stable error code for structured surfaces.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "scope_denied"
    }
}

/// Grant exactly the requested scopes that the connection holds, or deny with the missing
/// set. Pure and secret-free: it reasons over scope *labels* only (the `requires_scopes`
/// hints from t13) and never touches a [`Secret`].
///
/// `required` is the procedure's `requires_scopes`; `held` is the scope set the stored
/// connection was provisioned with. A driver calls this *before* using a fetched credential
/// so an under-scoped connection fails loudly with an actionable list rather than hitting a
/// 403 deep in a vendor call.
///
/// # Errors
/// [`ScopeError`] listing the missing scopes if any required scope is not held.
pub fn grant_scopes(required: &[String], held: &[String]) -> Result<ScopeGrant, ScopeError> {
    let missing: Vec<String> = required
        .iter()
        .filter(|r| !held.iter().any(|h| h == *r))
        .cloned()
        .collect();
    if missing.is_empty() {
        let granted: Vec<String> = required.to_vec();
        Ok(ScopeGrant { granted })
    } else {
        Err(ScopeError {
            missing,
            held: held.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::ConnectionId;

    fn key(driver: &str, connection: &str) -> CredentialKey {
        CredentialKey::new(
            DriverId::new(driver),
            ConnectionId::new(connection).unwrap(),
        )
    }

    #[test]
    fn secret_error_codes_and_messages_are_secret_free() {
        let nf = SecretError::NotFound(key("mail", "work"));
        assert_eq!(nf.code(), "secret_not_found");
        assert_eq!(nf.to_string(), "no credential for mail/work");

        assert_eq!(SecretError::Locked.code(), "secret_locked");

        let revoked = SecretError::Revoked(key("github", "team"));
        assert_eq!(revoked.code(), "secret_revoked");
        // The revoked error names the (driver, connection) selectors only — never a credential.
        assert_eq!(revoked.to_string(), "credential for github/team is revoked");

        let be = SecretError::Backend("reading credential blob".into());
        assert_eq!(be.code(), "secret_backend");
        // The backend message describes the operation, not any credential.
        assert!(be.to_string().contains("reading credential blob"));
    }

    #[test]
    fn grant_scopes_grants_when_all_held() {
        let required = vec!["mail.read".to_string(), "mail.send".to_string()];
        let held = vec![
            "mail.read".to_string(),
            "mail.send".to_string(),
            "mail.labels".to_string(),
        ];
        let grant = grant_scopes(&required, &held).unwrap();
        assert_eq!(grant.granted, required);
    }

    #[test]
    fn grant_scopes_denies_with_missing_set() {
        let required = vec!["mail.read".to_string(), "mail.send".to_string()];
        let held = vec!["mail.read".to_string()];
        let err = grant_scopes(&required, &held).unwrap_err();
        assert_eq!(err.code(), "scope_denied");
        assert_eq!(err.missing, vec!["mail.send".to_string()]);
        assert_eq!(err.held, vec!["mail.read".to_string()]);
        assert_eq!(
            err.to_string(),
            "connection lacks required scope(s): mail.send"
        );
    }

    #[test]
    fn grant_scopes_with_no_requirements_is_an_empty_grant() {
        let grant = grant_scopes(&[], &["x".to_string()]).unwrap();
        assert!(grant.granted.is_empty());
    }
}
