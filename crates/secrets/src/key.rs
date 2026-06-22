//! The owned, secret-free DTOs that key the store and describe an account: [`AccountId`],
//! [`CredentialKey`], and [`AccountRecord`] (RFD-0001 §9 — owned DTOs only, no vendor
//! type, no secret material crosses this boundary).
//!
//! The store is keyed by `(driver, account)`. Capability gating (§3) falls out of this
//! by construction: a [`CredentialKey`] names exactly one driver, so a driver that
//! resolves a key for its own [`DriverId`] can never reach another driver's credential —
//! cross-driver key access is impossible, not merely policed.
//!
//! Reuses [`cfs_types::DriverId`] (the canonical owned driver identity, a spine leaf)
//! rather than minting a second one, so the secrets surface speaks the same id the
//! Driver contract and the audit ledger do.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use cfs_types::DriverId;

/// A named account *within a driver*, e.g. `work`, `personal`, `prod`. Owned text; carries
/// no secret material (the account name is metadata, safe to log and to surface in a
/// resolution decision). Validated to be non-empty and free of the path/selector
/// separators so it can round-trip through a `(driver, account)` map and an `@account`
/// selector unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    /// Construct an account id, rejecting empties and reserved separators. Returns the
    /// owned id on success.
    ///
    /// # Errors
    /// [`AccountIdError`] if the name is empty or contains a reserved character
    /// (`@`, `/`, whitespace) that would collide with the `@account` selector or the
    /// store key encoding.
    pub fn new(id: impl Into<String>) -> Result<Self, AccountIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(AccountIdError::Empty);
        }
        if let Some(c) = id
            .chars()
            .find(|c| matches!(c, '@' | '/') || c.is_whitespace())
        {
            return Err(AccountIdError::ReservedChar(c));
        }
        Ok(Self(id))
    }

    /// The account id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for AccountId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an [`AccountId`] was rejected — structured and secret-free (an account *name* is
/// never a secret, but the error is structured for AI consumption all the same).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountIdError {
    /// The account id was empty.
    #[error("account id must not be empty")]
    Empty,
    /// The account id contained a reserved character that collides with the `@account`
    /// selector or the store key encoding.
    #[error("account id contains reserved character {0:?} (no '@', '/', or whitespace)")]
    ReservedChar(char),
}

/// The store key: a `(driver, account)` pair naming exactly one credential. This is the
/// capability boundary — a key cannot name "any driver", so a driver scoped to its own
/// [`DriverId`] cannot fetch another driver's secret (RFD §3 capability gating).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CredentialKey {
    /// The driver the credential belongs to, e.g. `mail`, `s3`.
    pub driver: DriverId,
    /// The named account within that driver, e.g. `work`.
    pub account: AccountId,
}

impl CredentialKey {
    /// Construct a credential key from a driver + account.
    #[must_use]
    pub fn new(driver: DriverId, account: AccountId) -> Self {
        Self { driver, account }
    }

    /// A stable, flat string encoding `driver/account` for backend keying (file map keys,
    /// env var suffixes). Both halves are validated to exclude `/`, so the join is
    /// unambiguous. Carries no secret material.
    #[must_use]
    pub fn flat(&self) -> String {
        format!("{}/{}", self.driver.as_str(), self.account.as_str())
    }
}

/// A listing entry describing one stored account — selectors + metadata only, **never**
/// the credential. Safe to `Debug`, serialize, log, and surface in `cfs account list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    /// The driver this account belongs to.
    pub driver: DriverId,
    /// The account name.
    pub account: AccountId,
    /// When the credential was stored (RFC 3339). Plaintext metadata for `list`/audit.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl AccountRecord {
    /// Construct a record. `created_at` is the caller's clock reading (the store stamps
    /// it on `put`); kept as an argument so this type performs no I/O and stays pure.
    #[must_use]
    pub fn new(driver: DriverId, account: AccountId, created_at: OffsetDateTime) -> Self {
        Self {
            driver,
            account,
            created_at,
        }
    }

    /// The `(driver, account)` key this record describes.
    #[must_use]
    pub fn key(&self) -> CredentialKey {
        CredentialKey::new(self.driver.clone(), self.account.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(s: &str) -> AccountId {
        AccountId::new(s).unwrap()
    }

    #[test]
    fn account_id_rejects_empty_and_reserved() {
        assert_eq!(AccountId::new(""), Err(AccountIdError::Empty));
        assert_eq!(
            AccountId::new("a@b"),
            Err(AccountIdError::ReservedChar('@'))
        );
        assert_eq!(
            AccountId::new("a/b"),
            Err(AccountIdError::ReservedChar('/'))
        );
        assert_eq!(
            AccountId::new("a b"),
            Err(AccountIdError::ReservedChar(' '))
        );
        assert_eq!(acct("work").as_str(), "work");
    }

    #[test]
    fn credential_key_flat_encoding_is_unambiguous() {
        let k = CredentialKey::new(DriverId::new("mail"), acct("work"));
        assert_eq!(k.flat(), "mail/work");
        // Both halves exclude '/', so exactly one split point exists.
        assert_eq!(k.flat().matches('/').count(), 1);
    }

    #[test]
    fn account_record_round_trips_through_serde_without_secrets() {
        let rec = AccountRecord::new(
            DriverId::new("s3"),
            acct("prod"),
            OffsetDateTime::UNIX_EPOCH,
        );
        let json = serde_json::to_string(&rec).unwrap();
        // Metadata only: driver, account, timestamp — nothing secret.
        assert!(json.contains("\"driver\""));
        assert!(json.contains("\"prod\""));
        let back: AccountRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
        assert_eq!(
            rec.key(),
            CredentialKey::new(DriverId::new("s3"), acct("prod"))
        );
    }
}
