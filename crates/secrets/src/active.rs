//! [`ActiveAccounts`] — the persistent `{driver -> active account}` map that backs
//! `cfs account use` and the "persistent active" rung of the resolution ladder.
//!
//! This is **plaintext metadata**, never a secret: it records *which* account is active
//! per driver (a selector), not any credential. It is therefore safe to serialize, log,
//! and store next to (but separate from) the encrypted credential blob. `account use` is
//! last-writer-wins and replayable (RFD §10 idempotency/recovery).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::key::{AccountId, DriverId};

/// A persistent map from driver to its active account. Owned, secret-free, serde-able.
///
/// In-memory only here (loading/saving the file lives in [`crate::LocalStore`]'s sibling
/// metadata path on native; the type itself does no I/O so it stays pure and wasm-safe).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAccounts {
    /// driver-id string -> active account. A `BTreeMap` for stable serialization order.
    active: BTreeMap<String, AccountId>,
}

impl ActiveAccounts {
    /// An empty active-account map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The active account for `driver`, if one has been chosen (`account use`).
    #[must_use]
    pub fn get(&self, driver: &DriverId) -> Option<&AccountId> {
        self.active.get(driver.as_str())
    }

    /// Set the active account for `driver` (last-writer-wins; replayable).
    pub fn set(&mut self, driver: &DriverId, account: AccountId) {
        self.active.insert(driver.as_str().to_string(), account);
    }

    /// Clear the active account for `driver` (e.g. after `account remove` of the active
    /// one). Idempotent.
    pub fn clear(&mut self, driver: &DriverId) {
        self.active.remove(driver.as_str());
    }

    /// Whether any active account is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(s: &str) -> AccountId {
        AccountId::new(s).unwrap()
    }

    #[test]
    fn set_get_clear_last_writer_wins() {
        let mut a = ActiveAccounts::new();
        let mail = DriverId::new("mail");
        assert!(a.get(&mail).is_none());

        a.set(&mail, acct("work"));
        assert_eq!(a.get(&mail), Some(&acct("work")));

        // Last-writer-wins.
        a.set(&mail, acct("personal"));
        assert_eq!(a.get(&mail), Some(&acct("personal")));

        a.clear(&mail);
        assert!(a.get(&mail).is_none());
        // Idempotent clear.
        a.clear(&mail);
    }

    #[test]
    fn round_trips_through_serde_as_plaintext_metadata() {
        let mut a = ActiveAccounts::new();
        a.set(&DriverId::new("mail"), acct("work"));
        a.set(&DriverId::new("s3"), acct("prod"));
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"work\""));
        let back: ActiveAccounts = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
