//! Two portable [`Secrets`] backends behind the one trait:
//!
//! - [`InMemoryStore`] — a process-local map; the test/CI default (no fs, no network, no
//!   real keychain) and the wasm-friendly base the `WorkerStore` builds on.
//! - [`EnvStore`] — read-only resolution from environment variables, the `12-factor` /
//!   CI / Cloudflare-`env`-binding case. A credential lives at
//!   `QFS_SECRET_<DRIVER>_<ACCOUNT>` (upper-cased); listing scans the process env.
//!
//! Both are pluggable behind [`Secrets`] exactly like [`crate::LocalStore`], proving the
//! backend abstraction: a driver depends on `&dyn Secrets` and never knows which one it
//! holds.

use std::collections::BTreeMap;
use std::sync::Mutex;

use time::OffsetDateTime;

use crate::key::{AccountId, AccountRecord, CredentialKey, DriverId};
use crate::secret::Secret;
use crate::store::{SecretError, Secrets};

/// One stored entry: the secret bytes plus the metadata timestamp. The bytes live in a
/// `Secret` so even the in-memory backend zeroizes on drop and never derives a leaking
/// `Debug` (the struct itself is not `Debug`).
struct Entry {
    value: Secret,
    created_at: OffsetDateTime,
}

/// A process-local, in-memory secret store — the test/CI default and the wasm base.
///
/// Holds entries in a `Mutex<BTreeMap>` so `&self` methods (the [`Secrets`] trait is
/// `&self`) can mutate, and the store is `Send + Sync`. No filesystem, no network, no OS
/// keychain — nothing to leak and nothing to mock.
#[derive(Default)]
pub struct InMemoryStore {
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl InMemoryStore {
    /// A fresh empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed an entry with an explicit timestamp — used by tests and by importers that
    /// carry their own clock. The trait's `put` stamps "now" instead.
    ///
    /// # Errors
    /// [`SecretError::Backend`] only if the internal lock is poisoned.
    pub fn insert_at(
        &self,
        key: &CredentialKey,
        value: Secret,
        created_at: OffsetDateTime,
    ) -> Result<(), SecretError> {
        let mut guard = self.lock()?;
        guard.insert(key.flat(), Entry { value, created_at });
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Entry>>, SecretError> {
        self.entries
            .lock()
            .map_err(|_| SecretError::Backend("in-memory store lock poisoned".into()))
    }
}

impl Secrets for InMemoryStore {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        let guard = self.lock()?;
        match guard.get(&key.flat()) {
            // Re-wrap the bytes (Secret is not Clone): copy the exposed bytes into a
            // fresh zeroizing Secret for the caller; the stored copy stays put.
            Some(entry) => Ok(Secret::new(entry.value.expose().to_vec())),
            None => Err(SecretError::NotFound(key.clone())),
        }
    }

    fn put(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError> {
        self.insert_at(key, value, OffsetDateTime::now_utc())
    }

    fn remove(&self, key: &CredentialKey) -> Result<(), SecretError> {
        let mut guard = self.lock()?;
        // Idempotent: removing an absent key is success.
        guard.remove(&key.flat());
        Ok(())
    }

    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<AccountRecord>, SecretError> {
        let guard = self.lock()?;
        let mut out = Vec::new();
        for (flat, entry) in guard.iter() {
            let Some(rec) = record_from_flat(flat, entry.created_at) else {
                continue;
            };
            if driver.is_none_or(|d| &rec.driver == d) {
                out.push(rec);
            }
        }
        Ok(out)
    }
}

/// Rebuild an [`AccountRecord`] from a `driver/account` flat key + its timestamp. Returns
/// `None` if the key does not split cleanly (defensive; keys are always well-formed here).
fn record_from_flat(flat: &str, created_at: OffsetDateTime) -> Option<AccountRecord> {
    let (driver, account) = flat.split_once('/')?;
    let account = AccountId::new(account).ok()?;
    Some(AccountRecord::new(
        DriverId::new(driver),
        account,
        created_at,
    ))
}

/// A read-only [`Secrets`] backend over environment variables — the 12-factor / CI /
/// Cloudflare-`env`-binding case. A credential for `(driver, account)` lives at
/// `QFS_SECRET_<DRIVER>_<ACCOUNT>` with both halves upper-cased.
///
/// `put`/`remove` are unsupported (the process env is read-only here); they return a
/// structured [`SecretError::Backend`] so a misuse fails loudly rather than silently
/// dropping a credential.
/// A pluggable "read one env var by name" function — the seam tests use to inject a
/// fixture map instead of mutating the real, global, thread-unsafe process environment.
type EnvReader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// A pluggable "enumerate the candidate var names" function, for `list`.
type EnvNames = Box<dyn Fn() -> Vec<String> + Send + Sync>;

pub struct EnvStore {
    prefix: String,
    /// The env reader. Pluggable so tests inject a fixture map instead of mutating the
    /// real process environment (which is global, racy, and unsafe under threads).
    reader: EnvReader,
    /// The full key namespace to scan for `list` (the var names that could exist).
    names: EnvNames,
}

impl EnvStore {
    /// An `EnvStore` reading the real process environment with the default `QFS_SECRET_`
    /// prefix.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            prefix: "QFS_SECRET_".to_string(),
            reader: Box::new(|name| std::env::var(name).ok()),
            names: Box::new(|| std::env::vars().map(|(k, _)| k).collect()),
        }
    }

    /// An `EnvStore` over an explicit `name -> value` fixture map — the test seam (no
    /// mutation of the real, global process environment).
    #[must_use]
    pub fn from_map(prefix: impl Into<String>, vars: BTreeMap<String, String>) -> Self {
        let prefix = prefix.into();
        let keys: Vec<String> = vars.keys().cloned().collect();
        Self {
            prefix,
            reader: Box::new(move |name| vars.get(name).cloned()),
            names: Box::new(move || keys.clone()),
        }
    }

    /// The env var name a `(driver, account)` credential is read from.
    #[must_use]
    pub fn var_name(&self, key: &CredentialKey) -> String {
        format!(
            "{}{}_{}",
            self.prefix,
            key.driver.as_str().to_uppercase(),
            key.account.as_str().to_uppercase()
        )
    }
}

impl Secrets for EnvStore {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        match (self.reader)(&self.var_name(key)) {
            Some(value) => Ok(Secret::from_string(value)),
            None => Err(SecretError::NotFound(key.clone())),
        }
    }

    fn put(&self, _key: &CredentialKey, _value: Secret) -> Result<(), SecretError> {
        Err(SecretError::Backend(
            "EnvStore is read-only: set the credential via the environment".into(),
        ))
    }

    fn remove(&self, _key: &CredentialKey) -> Result<(), SecretError> {
        Err(SecretError::Backend(
            "EnvStore is read-only: unset the credential in the environment".into(),
        ))
    }

    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<AccountRecord>, SecretError> {
        let want = driver.map(|d| d.as_str().to_uppercase());
        let mut out = Vec::new();
        for name in (self.names)() {
            let Some(rest) = name.strip_prefix(&self.prefix) else {
                continue;
            };
            // `<DRIVER>_<ACCOUNT>` — split on the first `_`. Driver/account are lower-cased
            // back for the record (we only have the upper-cased env form, so the record
            // reflects the canonical lower-case selector).
            let Some((drv, acct)) = rest.split_once('_') else {
                continue;
            };
            if let Some(w) = &want {
                if drv != w {
                    continue;
                }
            }
            let Ok(account) = AccountId::new(acct.to_lowercase()) else {
                continue;
            };
            out.push(AccountRecord::new(
                DriverId::new(drv.to_lowercase()),
                account,
                OffsetDateTime::UNIX_EPOCH,
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(driver: &str, account: &str) -> CredentialKey {
        CredentialKey::new(DriverId::new(driver), AccountId::new(account).unwrap())
    }

    /// In-memory backend round-trips put -> get -> remove, and a miss is a structured
    /// NotFound (no secret in the error).
    #[test]
    fn in_memory_round_trip_and_structured_miss() {
        let store = InMemoryStore::new();
        let k = key("mail", "work");

        // Miss before put.
        let miss = store.get(&k).unwrap_err();
        assert_eq!(miss.code(), "secret_not_found");

        store.put(&k, Secret::from("tok-work")).unwrap();
        let got = store.get(&k).unwrap();
        assert_eq!(got.expose_str(), Some("tok-work"));

        store.remove(&k).unwrap();
        assert_eq!(store.get(&k).unwrap_err().code(), "secret_not_found");
        // Idempotent remove.
        store.remove(&k).unwrap();
    }

    /// Multi-account listing: same driver, several accounts; `list(Some(driver))` filters.
    #[test]
    fn in_memory_lists_and_filters_by_driver() {
        let store = InMemoryStore::new();
        store.put(&key("mail", "work"), Secret::from("a")).unwrap();
        store
            .put(&key("mail", "personal"), Secret::from("b"))
            .unwrap();
        store.put(&key("s3", "prod"), Secret::from("c")).unwrap();

        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 3);

        let mail = store.list(Some(&DriverId::new("mail"))).unwrap();
        assert_eq!(mail.len(), 2);
        assert!(mail.iter().all(|r| r.driver == DriverId::new("mail")));
    }

    /// Env backend resolves from a fixture map (never the real process env) and reports
    /// read-only writes as structured errors.
    #[test]
    fn env_store_reads_fixture_map_and_is_read_only() {
        let mut vars = BTreeMap::new();
        vars.insert("QFS_SECRET_MAIL_WORK".to_string(), "env-tok".to_string());
        vars.insert("QFS_SECRET_S3_PROD".to_string(), "s3-tok".to_string());
        let store = EnvStore::from_map("QFS_SECRET_", vars);

        assert_eq!(store.var_name(&key("mail", "work")), "QFS_SECRET_MAIL_WORK");
        assert_eq!(
            store.get(&key("mail", "work")).unwrap().expose_str(),
            Some("env-tok")
        );
        assert_eq!(
            store.get(&key("mail", "missing")).unwrap_err().code(),
            "secret_not_found"
        );

        // Read-only: writes fail with a structured backend error, no panic.
        assert_eq!(
            store
                .put(&key("mail", "work"), Secret::from("x"))
                .unwrap_err()
                .code(),
            "secret_backend"
        );

        // list reconstructs the (driver, account) selectors from the env namespace.
        let recs = store.list(Some(&DriverId::new("mail"))).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].account.as_str(), "work");
    }
}
