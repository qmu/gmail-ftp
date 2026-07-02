//! `SECRET env:<VAR>` / `SECRET vault:<driver>/<connection>` reference resolution (the connection
//! epic — `20260630004100`).
//!
//! A `CREATE CONNECTION` declaration names *where* a secret lives, never the value. This module
//! turns that reference into a [`Secret`] at **use time** — lazily (declaring or `describe`-ing a
//! connection resolves nothing), secret-free on failure (the error names the offending VAR / vault
//! path / scheme, never a value, and carries a stable `code`), and never logging the value.
//!
//! An inline literal is **rejected**: a secret never sits in a statement. Only `env:` and `vault:`
//! references are accepted. `vault:` reads the same envelope-encrypted store `qfs account add`
//! writes (so that command stays the secret *store* behind a `SECRET vault:…` reference); the caller
//! owns opening it (and thus the `QFS_PASSPHRASE`), so a locked/absent store fails closed.

use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secret, SecretError, Secrets};

/// A failure to resolve a `SECRET` reference. Secret-free by construction; carries a stable `code`.
#[derive(Debug)]
pub enum SecretRefError {
    /// An `env:<VAR>` whose environment variable is unset or empty.
    EnvMissing {
        /// The variable name (non-secret).
        var: String,
    },
    /// The reference uses no known scheme — e.g. an inline literal, which is never accepted.
    BadScheme,
    /// A `vault:<ref>` whose body is not the required `<driver>/<connection>` shape.
    BadVaultRef,
    /// The credential store rejected the lookup (locked / not found / backend).
    Store(SecretError),
}

impl SecretRefError {
    /// A stable, secret-free error code for the CLI/JSON envelope.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SecretRefError::EnvMissing { .. } => "secret_env_missing",
            SecretRefError::BadScheme => "secret_bad_ref",
            SecretRefError::BadVaultRef => "secret_bad_vault_ref",
            SecretRefError::Store(e) => e.code(),
        }
    }
}

impl std::fmt::Display for SecretRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretRefError::EnvMissing { var } => {
                write!(f, "the SECRET env var `{var}` is unset or empty")
            }
            SecretRefError::BadScheme => write!(
                f,
                "a SECRET reference must be `env:<VAR>` or `vault:<driver>/<connection>` \
                 (an inline secret value is never accepted)"
            ),
            SecretRefError::BadVaultRef => {
                write!(
                    f,
                    "a `vault:` reference must be `vault:<driver>/<connection>`"
                )
            }
            SecretRefError::Store(e) => write!(f, "credential store: {e}"),
        }
    }
}

impl std::error::Error for SecretRefError {}

/// Resolve a `SECRET` reference string to a [`Secret`]:
/// - `env:<VAR>` reads the environment variable (unset/empty → a structured error);
/// - `vault:<driver>/<connection>` reads `vault` (the envelope-encrypted store, passed in so the
///   caller owns the passphrase/open — a locked/absent store fails closed via [`SecretError`]).
///
/// The value is a redacting, zeroized [`Secret`] — it never enters a DTO, a log, or a `describe`.
///
/// # Errors
/// [`SecretRefError`] (secret-free, stable `code`) for an unknown scheme / inline literal, a missing
/// env var, a malformed `vault:` ref, or a store failure (locked / not found).
pub fn resolve_secret_ref(reference: &str, vault: &dyn Secrets) -> Result<Secret, SecretRefError> {
    if let Some(var) = reference.strip_prefix("env:") {
        let value = std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| SecretRefError::EnvMissing {
                var: var.to_string(),
            })?;
        Ok(Secret::from_string(value))
    } else if let Some(body) = reference.strip_prefix("vault:") {
        let (driver, connection) = body.split_once('/').ok_or(SecretRefError::BadVaultRef)?;
        if driver.is_empty() || connection.is_empty() {
            return Err(SecretRefError::BadVaultRef);
        }
        let conn = ConnectionId::new(connection).map_err(|_| SecretRefError::BadVaultRef)?;
        let key = CredentialKey::new(DriverId(driver.to_string()), conn);
        vault.get(&key).map_err(SecretRefError::Store)
    } else {
        Err(SecretRefError::BadScheme)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use qfs_secrets::InMemoryStore;
    use time::OffsetDateTime;

    /// An empty vault for the tests that never reach it (env / bad-scheme paths).
    fn empty_vault() -> InMemoryStore {
        InMemoryStore::new()
    }

    #[test]
    fn env_reference_reads_the_variable() {
        // A unique var name so parallel tests never collide.
        let var = "QFS_SECRET_REF_TEST_ENV_HIT";
        std::env::set_var(var, "s3cr3t");
        let got = resolve_secret_ref(&format!("env:{var}"), &empty_vault()).unwrap();
        assert_eq!(got.expose_str(), Some("s3cr3t"));
        std::env::remove_var(var);
    }

    #[test]
    fn a_missing_env_reference_is_a_structured_secret_free_error() {
        let var = "QFS_SECRET_REF_TEST_ENV_MISS";
        std::env::remove_var(var);
        let err = resolve_secret_ref(&format!("env:{var}"), &empty_vault()).unwrap_err();
        assert_eq!(err.code(), "secret_env_missing");
        // The error names the VAR, never a value.
        assert!(err.to_string().contains(var));
    }

    #[test]
    fn an_inline_literal_is_rejected() {
        let err = resolve_secret_ref("hunter2", &empty_vault()).unwrap_err();
        assert_eq!(err.code(), "secret_bad_ref");
    }

    #[test]
    fn a_malformed_vault_reference_is_rejected() {
        let err = resolve_secret_ref("vault:gmail", &empty_vault()).unwrap_err();
        assert_eq!(err.code(), "secret_bad_vault_ref");
    }

    #[test]
    fn a_vault_reference_reads_the_seeded_store() {
        let store = InMemoryStore::new();
        let key = CredentialKey::new(
            DriverId("gmail".to_string()),
            ConnectionId::new("work").unwrap(),
        );
        store
            .insert_at(
                &key,
                Secret::from_string("tok-work".to_string()),
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
        let got = resolve_secret_ref("vault:gmail/work", &store).unwrap();
        assert_eq!(got.expose_str(), Some("tok-work"));
    }

    #[test]
    fn a_vault_miss_fails_closed_with_a_store_code() {
        let err = resolve_secret_ref("vault:gmail/absent", &empty_vault()).unwrap_err();
        assert_eq!(err.code(), "secret_not_found");
    }

    #[test]
    fn the_resolved_secret_never_appears_in_debug_output() {
        let var = "QFS_SECRET_REF_TEST_DEBUG";
        std::env::set_var(var, "do-not-leak");
        let got = resolve_secret_ref(&format!("env:{var}"), &empty_vault()).unwrap();
        assert!(
            !format!("{got:?}").contains("do-not-leak"),
            "a Secret must redact its value in Debug"
        );
        std::env::remove_var(var);
    }
}
