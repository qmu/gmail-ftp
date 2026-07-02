//! `qfs-secrets` — the credential / secret store + multi-connection resolution (RFD-0001 §10).
//!
//! `qfs` is one binary holding tokens for Gmail, Drive, S3/R2, D1, GitHub, Slack, AWS and
//! Cloudflare while running cross-service effect-plans — a large blast radius. This crate
//! is the **single secrets surface** every driver and the server read from:
//!
//! - [`Secret`] — the **only** type holding live key material; redacting `Debug`/`Display`,
//!   no `Clone`/`Serialize`, zeroized on drop, value reachable only via [`Secret::expose`].
//!   Redaction is the headline invariant (see `secret.rs` and the [`tests`] below).
//! - [`Secrets`] — the one trait drivers + server call ([`get`]/[`put`]/[`remove`]/[`list`]),
//!   keyed by [`CredentialKey`] = `(driver, connection)`. Cross-driver access is impossible by
//!   construction (a key names exactly one driver).
//!   [`get`]: Secrets::get [`put`]: Secrets::put [`remove`]: Secrets::remove [`list`]: Secrets::list
//! - Backends behind that one trait: [`InMemoryStore`] (test/CI/wasm base), [`EnvStore`]
//!   (12-factor / CI / CF `env` bindings), [`LocalStore`] (native encrypted-at-rest,
//!   `0600`, AEAD, atomic write), and `WorkerStore` (wasm Secret Store).
//! - [`resolve`] — the connection-resolution ladder
//!   (`--connection` > `AT 'acct'` > active > sole > structured error), recording the chosen
//!   [`ConnectionSource`] for the audit ledger ("who ran as whom") — never the credential.
//! - [`grant_scopes`] — the scope tie-in: a driver requests a credential *with* required
//!   scopes (the `requires_scopes` hints from t13) and gets a structured, secret-free
//!   grant/deny.
//!
//! ## Purity / boundary discipline
//! Owned-DTO only; reuses [`qfs_types::DriverId`] and depends on no other workspace crate,
//! so the spine stays acyclic (`qfs-secrets -> qfs-types`). Resolution ([`resolve`]) and
//! the scope check ([`grant_scopes`]) are pure; the only I/O is reading/writing bytes in a
//! backend, deliberately behind the [`Secrets`] trait so a `Plan` never embeds a secret —
//! only an connection *selector* (RFD §3 purity invariant).
//!
//! ## wasm-friendliness
//! [`LocalStore`] is `cfg(not(target_arch = "wasm32"))` (no fs on Workers); the wasm build
//! uses `WorkerStore` instead. The trait + DTOs + [`Secret`] + [`resolve`] compile on both.
//! The [`envelope`] primitive (t43) is likewise `cfg(not(target_arch = "wasm32"))`: its AEAD/KDF
//! code is pure Rust, but its `rand`/`getrandom` CSPRNG has no default Workers backend, and the
//! SQLite store that consumes it lives in the (native) binary — Workers use `WorkerStore` and never
//! need the envelope, so confining it keeps qfs-secrets wasm-buildable (the documented invariant).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod active;
mod backends;
// t54 (roadmap M4): the PURE cloud-driver consent / sign-in decision (no I/O, no Secret), so it
// builds on both native and wasm. The binary wires the real identity + consent state into it.
mod consent;
// t80 (roadmap M5, decision U): the PURE end-to-end attendance gate (no I/O, no crypto, no Secret) —
// refuses an E2E/high-sensitivity connection for an unattended autonomous agent. Builds on both
// native and wasm; the per-recipient wrap PRIMITIVE lives in qfs-oauth (it needs p256 ECDH, which
// must NOT enter this leaf), the per-recipient STORE binary-side; this leaf holds only the decision.
mod e2e;
// t43 envelope crypto: native-only (its CSPRNG has no default Workers backend; the SQLite store
// that consumes it lives in the native binary). Keeps qfs-secrets wasm-buildable.
#[cfg(not(target_arch = "wasm32"))]
mod envelope;
// ADR 0008 §5 (KeyGuardian): the PURE vault-key-slot model — LUKS-style N wraps of one DEK, one
// per guardian. Builds on envelope, so native-only like it; the guardian I/O (keyring, prompt,
// env) lives in the binary and passes a resolver in.
mod key;
mod resolve;
mod secret;
#[cfg(not(target_arch = "wasm32"))]
mod slots;
// t81 (roadmap M5): the PURE shared-connection USE gate (no I/O, no Secret), so it builds on both
// native and wasm. The binary wires the real owner + actor-policy-grant state into it.
mod shared;
mod store;

#[cfg(not(target_arch = "wasm32"))]
mod local;
#[cfg(target_arch = "wasm32")]
mod worker;

pub use active::ActiveConnections;
pub use backends::{EnvStore, InMemoryStore};
// t54 (roadmap M4): the cloud-driver consent / sign-in decision. The binary's `connection add`/`use`
// gate and its commit-time bind both consult these to fail closed for an unauthenticated operator.
pub use consent::{bind_gate, is_cloud_driver, ConsentError, CLOUD_DRIVERS};
// t80 (roadmap M5, decision U): the end-to-end attendance gate. The binary's commit-time bind
// consults this to fail closed for a high-sensitivity (per-recipient-wrapped) connection used by an
// autonomous agent unattended — it requires a human recipient unwrap in the loop.
pub use e2e::{e2e_attendance_gate, E2eUseError};
// The envelope-encryption primitive (t43): the SQLite credential store (in the binary) builds on
// these — a passphrase-derived KEK wraps a random DEK that seals each secret value. Native-only
// (see the `mod envelope` gate above); Workers never need it.
#[cfg(not(target_arch = "wasm32"))]
pub use envelope::{
    derive_kek, generate_dek, generate_salt, open, rewrap_dek, seal, unwrap_dek, wrap_dek,
    EnvelopeError,
};
// ADR 0008 §5 (KeyGuardian): the vault-key-slot model. The binary's credential store unlocks the
// DEK through the slot set (any one guardian suffices) and enrolls/revokes wraps without touching
// a sealed value. Native-only (see the `mod slots` gate above).
pub use key::{
    ConnectionId, ConnectionIdError, ConnectionRecord, CredentialKey, DriverId, OwnerScope,
};
pub use resolve::{resolve, ConnectionSource, Resolution, ResolveError};
pub use secret::{Secret, REDACTED};
#[cfg(not(target_arch = "wasm32"))]
pub use slots::{unlock_via_slots, SlotWrap};
// t81 (roadmap M5): the shared-connection USE gate. The binary's commit-time bind consults this to
// fail closed for a member whose actor-policy does not grant a project-owned connection's scope.
pub use shared::{shared_use_gate, SharedUseError};
pub use store::{grant_scopes, ScopeError, ScopeGrant, SecretError, Secrets};

#[cfg(not(target_arch = "wasm32"))]
pub use local::{default_credentials_path, load_or_create_salt, LocalStore};
#[cfg(target_arch = "wasm32")]
pub use worker::WorkerStore;

#[cfg(test)]
mod tests {
    use super::*;

    /// A planted credential value, unmistakable if it ever surfaces in output.
    const PLANTED: &str = "PLANTED-LEAK-CANARY-9f8e7d6c5b4a";

    /// THE headline redaction invariant, asserted end-to-end across every text surface a
    /// secret could escape through: `Debug`, `Display`, and **every error type** that
    /// could conceivably carry one. A planted secret never appears in any of them.
    #[test]
    fn a_planted_secret_never_appears_in_any_error_or_log_surface() {
        let secret = Secret::from(PLANTED);

        // 1. Debug / Display of the Secret itself.
        let surfaces = vec![format!("{secret:?}"), format!("{secret}")];

        // 2. The store NotFound error carries the KEY (selectors), never the value — and
        //    we feed a store that does not have the secret to exercise the miss path.
        let key = CredentialKey::new(DriverId::new("mail"), ConnectionId::new("work").unwrap());
        let store = InMemoryStore::new();
        let not_found = store.get(&key).unwrap_err();

        // 3. Put the planted secret in, then drive every OTHER error surface that exists
        //    so we prove none of them can be constructed from / leak the value.
        store.put(&key, Secret::from(PLANTED)).unwrap();

        let backend = SecretError::Backend("reading credential blob".into());
        let locked = SecretError::Locked;
        let revoked = SecretError::Revoked(key.clone());
        let scope =
            grant_scopes(&["mail.send".to_string()], &["mail.read".to_string()]).unwrap_err();
        let ambiguous = {
            // Two connections, no selector -> Ambiguous; lists connection NAMES, not secrets.
            store
                .put(
                    &CredentialKey::new(DriverId::new("mail"), ConnectionId::new("home").unwrap()),
                    Secret::from(PLANTED),
                )
                .unwrap();
            let available = store.list(Some(&DriverId::new("mail"))).unwrap();
            resolve(
                &DriverId::new("mail"),
                None,
                None,
                &ActiveConnections::new(),
                &available,
            )
            .unwrap_err()
        };

        let mut all = surfaces;
        all.push(format!("{not_found:?}"));
        all.push(not_found.to_string());
        all.push(format!("{backend:?}"));
        all.push(backend.to_string());
        all.push(format!("{locked:?}"));
        all.push(locked.to_string());
        all.push(format!("{revoked:?}"));
        all.push(revoked.to_string());
        all.push(format!("{scope:?}"));
        all.push(scope.to_string());
        all.push(format!("{ambiguous:?}"));
        all.push(ambiguous.to_string());

        for s in &all {
            assert!(
                !s.contains(PLANTED),
                "SECRET LEAK: planted value surfaced in: {s}"
            );
            assert!(
                !s.contains("9f8e7d6c5b4a"),
                "SECRET LEAK: fragment of planted value surfaced in: {s}"
            );
        }

        // Sanity: the redaction marker DID render where the secret was formatted.
        assert!(all.iter().any(|s| s.contains(REDACTED)));
    }

    /// End-to-end multi-connection flow over the in-memory backend + resolver + scope grant,
    /// proving the pieces compose: store two connections, resolve by each rung, grant scopes.
    #[test]
    fn end_to_end_multi_connection_resolution_and_scope_grant() {
        let store = InMemoryStore::new();
        let mail = DriverId::new("mail");
        let work = CredentialKey::new(mail.clone(), ConnectionId::new("work").unwrap());
        let home = CredentialKey::new(mail.clone(), ConnectionId::new("home").unwrap());
        store.put(&work, Secret::from("tok-work")).unwrap();
        store.put(&home, Secret::from("tok-home")).unwrap();

        let available = store.list(Some(&mail)).unwrap();
        assert_eq!(available.len(), 2);

        // Ambiguous without a selector.
        assert_eq!(
            resolve(&mail, None, None, &ActiveConnections::new(), &available)
                .unwrap_err()
                .code(),
            "connection_ambiguous"
        );

        // Flag selects, then fetch that connection's secret.
        let chosen = ConnectionId::new("work").unwrap();
        let r = resolve(
            &mail,
            Some(&chosen),
            None,
            &ActiveConnections::new(),
            &available,
        )
        .unwrap();
        assert_eq!(r.source, ConnectionSource::Flag);
        let key = CredentialKey::new(mail.clone(), r.connection);
        assert_eq!(store.get(&key).unwrap().expose_str(), Some("tok-work"));

        // Scope grant for the resolved connection.
        let grant = grant_scopes(
            &["mail.read".to_string()],
            &["mail.read".to_string(), "mail.send".to_string()],
        )
        .unwrap();
        assert_eq!(grant.granted, vec!["mail.read".to_string()]);
    }

    /// The Secrets trait is object-safe (`Arc<dyn Secrets>`) so the engine can thread one
    /// handle through the driver-bind context regardless of the concrete backend.
    #[test]
    fn secrets_is_object_safe_across_backends() {
        let handles: Vec<std::sync::Arc<dyn Secrets>> = vec![
            std::sync::Arc::new(InMemoryStore::new()),
            std::sync::Arc::new(EnvStore::from_map("QFS_SECRET_", Default::default())),
        ];
        let key = CredentialKey::new(DriverId::new("mail"), ConnectionId::new("work").unwrap());
        for h in &handles {
            // A miss is a structured NotFound on every backend.
            assert_eq!(h.get(&key).unwrap_err().code(), "secret_not_found");
        }
    }
}
