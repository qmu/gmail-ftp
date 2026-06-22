//! [`WorkerStore`] — the `wasm32` (Cloudflare Workers) [`Secrets`] backend (RFD-0001 §10).
//!
//! On Workers there is **no filesystem** — so [`crate::LocalStore`]'s `0600`/AEAD-file
//! path is compiled out entirely (see `local.rs`'s `#![cfg(not(target_arch = "wasm32"))]`)
//! and credentials come from the Worker's Secret Store / `env` bindings instead. This
//! store is a thin adapter over a caller-supplied resolver closure that reads a binding by
//! name; the host runtime (the `cfs serve` Worker entrypoint, a later ticket) wires the
//! actual `env.get(...)` call.
//!
//! Compiled only on `wasm32`. The trait surface is identical to every other backend, so a
//! driver depends on `&dyn Secrets` and is oblivious to which target it runs on.

#![cfg(target_arch = "wasm32")]

use crate::key::{AccountRecord, CredentialKey, DriverId};
use crate::secret::Secret;
use crate::store::{SecretError, Secrets};

/// Resolves a Worker binding name to its value — the host (`cfs serve` Worker entrypoint)
/// supplies this, wrapping the actual `env.get(...)` call.
pub type BindingResolver = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A [`Secrets`] backend over a Worker binding resolver. The closure maps a binding name
/// (`CFS_SECRET_<DRIVER>_<ACCOUNT>`) to its value, exactly as the host's Secret Store /
/// `env` exposes it. No filesystem, no `0600` — confidentiality is the platform's.
pub struct WorkerStore {
    prefix: String,
    resolver: BindingResolver,
}

impl WorkerStore {
    /// Build a store over a binding resolver. `prefix` is prepended to the
    /// `<DRIVER>_<ACCOUNT>` binding name (default callers pass `"CFS_SECRET_"`).
    #[must_use]
    pub fn new(prefix: impl Into<String>, resolver: BindingResolver) -> Self {
        Self {
            prefix: prefix.into(),
            resolver,
        }
    }

    /// The binding name a `(driver, account)` credential is read from.
    #[must_use]
    pub fn binding_name(&self, key: &CredentialKey) -> String {
        format!(
            "{}{}_{}",
            self.prefix,
            key.driver.as_str().to_uppercase(),
            key.account.as_str().to_uppercase()
        )
    }
}

impl Secrets for WorkerStore {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        match (self.resolver)(&self.binding_name(key)) {
            Some(v) => Ok(Secret::from_string(v)),
            None => Err(SecretError::NotFound(key.clone())),
        }
    }

    fn put(&self, _key: &CredentialKey, _value: Secret) -> Result<(), SecretError> {
        Err(SecretError::Backend(
            "WorkerStore is read-only: provision the secret via the Worker Secret Store".into(),
        ))
    }

    fn remove(&self, _key: &CredentialKey) -> Result<(), SecretError> {
        Err(SecretError::Backend(
            "WorkerStore is read-only: remove the secret via the Worker Secret Store".into(),
        ))
    }

    fn list(&self, _driver: Option<&DriverId>) -> Result<Vec<AccountRecord>, SecretError> {
        // Worker bindings are not enumerable from inside the guest; listing is an
        // out-of-band (dashboard / API) concern. Return empty rather than error so a
        // generic `cfs account list` does not break on Workers.
        Ok(Vec::new())
    }
}
