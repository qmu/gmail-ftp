//! t56: the **rusqlite upstream-OIDC-provider store** over the System DB.
//!
//! This lives in `qfs-store` (not in `qfs-oauth`) on purpose: `qfs-oauth` is a pure-ish leaf with NO
//! rusqlite/tokio, and `qfs-store` is the crate that owns a real `rusqlite::Connection`. So the OIDC
//! verification/linking logic stays in `qfs-oauth`/`qfs-identity` and the upstream-PROVIDER
//! registration persistence is here — the same split t48 uses for the OAuth signing keys
//! (`OauthKeyStore`) and t43 uses for the secret store (`SqliteSecrets`).
//!
//! ## What it stores (the "hub model" RP registrations)
//! One row per UPSTREAM IdP a host trusts for human login (decision D, §4.1): the issuer, the RP
//! `client_id` qfs presents to the upstream, the cached discovery endpoints + JWKS (so the verifier
//! resolves the upstream signing key offline), and the upstream **client secret** — which is the one
//! secret here and is ENVELOPE-ENCRYPTED at rest.
//!
//! ## Envelope at rest (decision E, mirroring t48's `OauthKeyStore`)
//! On first open the store generates a random System-DB data-key (DEK), derives a KEK from
//! `QFS_PASSPHRASE` + a fresh argon2id salt, wraps the DEK, and records `(wrapped_dek, kdf_salt)` once
//! in `oidc_provider_meta`. Each upstream client secret is sealed under the DEK
//! (ChaCha20-Poly1305, fresh nonce) into `oidc_providers.client_secret_encrypted`. Reopening
//! re-derives the KEK and unwraps the SAME DEK; a wrong passphrase fails to unwrap →
//! [`StoreError::Locked`]. The discovery/JWKS columns are stored in the clear (they are PUBLIC).
//!
//! ## Secret hygiene (RFD §10)
//! The DEK, the KEK, and the decrypted client secret are NEVER logged or formatted. A decrypted
//! secret leaves this store ONLY inside a redacting, zeroized [`Secret`]. Every error is secret-free.

use std::sync::Mutex;

use qfs_secrets::{
    derive_kek, generate_dek, generate_salt, open, seal, unwrap_dek, wrap_dek, Secret,
};
use rusqlite::{Connection, OptionalExtension};

use crate::{Db, StoreError};

/// The AEAD nonce width ChaCha20-Poly1305 uses (must match `qfs_secrets::envelope`). A sealed client
/// secret is stored as `nonce(12) || ciphertext` in one BLOB column.
const NONCE_LEN: usize = 12;

/// A registered upstream OIDC provider as it is written/read (with the client secret in the clear in
/// memory, wrapped in a redacting [`Secret`]). The clear discovery/JWKS columns let the binary's RP
/// verifier resolve the upstream signing key offline; the `client_secret` is sealed at rest.
pub struct OidcProviderRecord {
    /// The LOCAL provider key — an operator label (`google`) or the issuer URL. The `provider` half
    /// of an `accounts(provider, subject)` link.
    pub provider: String,
    /// The upstream issuer (`iss`) the RP verifier checks the ID token's `iss` against.
    pub issuer: String,
    /// The RP client id qfs presents to the upstream (and the expected `aud` in the ID token).
    pub client_id: String,
    /// The upstream client secret, decrypted into a redacting [`Secret`]; `None` for a public
    /// (PKCE-only) client. Never logged/serialized.
    pub client_secret: Option<Secret>,
    /// OUR RP callback the upstream redirects back to (`redirect_uri`), if configured.
    pub redirect_uri: Option<String>,
    /// The space-delimited scope set requested at the upstream (default `openid email profile`).
    pub scopes: String,
    /// The upstream authorization endpoint (cached from discovery), if known.
    pub authorization_endpoint: Option<String>,
    /// The upstream token endpoint (cached from discovery), if known.
    pub token_endpoint: Option<String>,
    /// The upstream JWKS URI (cached from discovery), if known.
    pub jwks_uri: Option<String>,
    /// The cached upstream JWKS JSON (the published verification keys), if fetched.
    pub jwks_json: Option<String>,
}

/// A new/updated upstream-provider registration the binary hands in (the client secret as a redacting
/// [`Secret`], sealed before it touches the DB). Field-for-field the input to
/// [`SqliteOidcProviderStore::upsert_provider`].
pub struct NewOidcProvider<'a> {
    /// The LOCAL provider key (see [`OidcProviderRecord::provider`]).
    pub provider: &'a str,
    /// The upstream issuer.
    pub issuer: &'a str,
    /// The RP client id.
    pub client_id: &'a str,
    /// The upstream client secret to seal at rest; `None` for a public client.
    pub client_secret: Option<&'a Secret>,
    /// OUR RP callback (`redirect_uri`).
    pub redirect_uri: Option<&'a str>,
    /// The requested scope set (the caller passes the default if unset).
    pub scopes: &'a str,
    /// The cached upstream authorization endpoint.
    pub authorization_endpoint: Option<&'a str>,
    /// The cached upstream token endpoint.
    pub token_endpoint: Option<&'a str>,
    /// The cached upstream JWKS URI.
    pub jwks_uri: Option<&'a str>,
    /// The cached upstream JWKS JSON.
    pub jwks_json: Option<&'a str>,
}

/// The System-DB-backed upstream-OIDC-provider store. Owns the migrated connection inside a `Mutex`
/// (so the backend is `Send + Sync`) plus the unwrapped System-DB data-key held only in process
/// memory. Never `Debug` (it holds key material).
pub struct SqliteOidcProviderStore {
    conn: Mutex<Connection>,
    /// The unwrapped 32-byte System-DB data-key. Seals/opens every upstream client secret; never raw.
    dek: [u8; 32],
}

impl SqliteOidcProviderStore {
    /// Open the store over a migrated System-DB `conn`, unlocking (or initializing) the envelope with
    /// `passphrase`. The OIDC-providers migration (v9) must already be applied — `SystemDb::open` does
    /// that on start.
    ///
    /// - First open (no `oidc_provider_meta` row): generate a DEK, derive a KEK from `passphrase` + a
    ///   fresh salt, wrap the DEK, INSERT the single meta row.
    /// - Subsequent opens: read `(wrapped_dek, kdf_salt)`, re-derive the KEK, unwrap the same DEK.
    ///
    /// # Errors
    /// [`StoreError::Locked`] if the passphrase is wrong or the meta row is tampered;
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn open_or_init(conn: Connection, passphrase: &Secret) -> Result<Self, StoreError> {
        let meta: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT wrapped_dek, kdf_salt FROM oidc_provider_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?;

        let dek = match meta {
            // Established store: re-derive the KEK and unwrap the SAME DEK. Wrong passphrase /
            // tampered meta fails authentication -> Locked (no bytes leaked).
            Some((wrapped, salt)) => {
                let kek = derive_kek(passphrase.expose(), &salt).map_err(|_| StoreError::Locked)?;
                unwrap_dek(&kek, &wrapped).map_err(|_| StoreError::Locked)?
            }
            // Fresh store: mint a DEK + salt, wrap under the passphrase KEK, persist once.
            None => {
                let dek = generate_dek();
                let salt = generate_salt();
                let kek = derive_kek(passphrase.expose(), &salt).map_err(|_| StoreError::Locked)?;
                let wrapped = wrap_dek(&kek, &dek).map_err(|_| {
                    StoreError::Sqlite("wrapping the oidc data key failed".to_string())
                })?;
                conn.execute(
                    "INSERT INTO oidc_provider_meta (id, wrapped_dek, kdf_salt) VALUES (1, ?1, ?2)",
                    rusqlite::params![wrapped, salt.as_slice()],
                )
                .map_err(StoreError::from)?;
                dek
            }
        };

        Ok(Self {
            conn: Mutex::new(conn),
            dek,
        })
    }

    /// Build the store over a migrated [`Db`] handle (consumes it for the owned connection).
    ///
    /// # Errors
    /// As [`SqliteOidcProviderStore::open_or_init`].
    pub fn from_db(db: Db, passphrase: &Secret) -> Result<Self, StoreError> {
        Self::open_or_init(db.into_connection(), passphrase)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Sqlite("oidc provider store lock poisoned".to_string()))
    }

    /// Register (or replace) an upstream provider: seal the client secret under the DEK and UPSERT the
    /// row keyed by `provider`. Re-registering the same `provider` overwrites it (e.g. to refresh the
    /// cached discovery/JWKS or rotate the secret).
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a seal / DB failure.
    pub fn upsert_provider(&self, p: &NewOidcProvider<'_>) -> Result<(), StoreError> {
        let sealed = match p.client_secret {
            Some(secret) => Some(self.seal(secret)?),
            None => None,
        };
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oidc_providers (provider, issuer, client_id, client_secret_encrypted, \
             redirect_uri, scopes, authorization_endpoint, token_endpoint, jwks_uri, jwks_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(provider) DO UPDATE SET \
             issuer = excluded.issuer, client_id = excluded.client_id, \
             client_secret_encrypted = excluded.client_secret_encrypted, \
             redirect_uri = excluded.redirect_uri, scopes = excluded.scopes, \
             authorization_endpoint = excluded.authorization_endpoint, \
             token_endpoint = excluded.token_endpoint, jwks_uri = excluded.jwks_uri, \
             jwks_json = excluded.jwks_json",
            rusqlite::params![
                p.provider,
                p.issuer,
                p.client_id,
                sealed,
                p.redirect_uri,
                p.scopes,
                p.authorization_endpoint,
                p.token_endpoint,
                p.jwks_uri,
                p.jwks_json,
            ],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// Look up a registered upstream provider by its local `provider` key, decrypting the client
    /// secret into a [`Secret`]. `Ok(None)` when no such provider is registered.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB / decrypt failure (the DEK is valid — unwrapped on open — so a
    /// decrypt failure means a corrupt/tampered column, reported secret-free).
    pub fn get_provider(&self, provider: &str) -> Result<Option<OidcProviderRecord>, StoreError> {
        let conn = self.lock()?;
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            String,
            Option<Vec<u8>>,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = conn
            .query_row(
                "SELECT issuer, client_id, client_secret_encrypted, redirect_uri, scopes, \
                 authorization_endpoint, token_endpoint, jwks_uri, jwks_json \
                 FROM oidc_providers WHERE provider = ?1",
                rusqlite::params![provider],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some((
            issuer,
            client_id,
            sealed,
            redirect_uri,
            scopes,
            authorization_endpoint,
            token_endpoint,
            jwks_uri,
            jwks_json,
        )) = row
        else {
            return Ok(None);
        };
        let client_secret = match sealed {
            Some(bytes) => Some(self.unseal(&bytes)?),
            None => None,
        };
        Ok(Some(OidcProviderRecord {
            provider: provider.to_string(),
            issuer,
            client_id,
            client_secret,
            redirect_uri,
            scopes,
            authorization_endpoint,
            token_endpoint,
            jwks_uri,
            jwks_json,
        }))
    }

    /// The local `provider` keys of every registered upstream (for an operator listing). No secret
    /// material crosses this — just the labels.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn list_providers(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT provider FROM oidc_providers ORDER BY provider")
            .map_err(StoreError::from)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(StoreError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(StoreError::from)?);
        }
        Ok(out)
    }

    /// Seal a client secret under the DEK, returning `nonce || ciphertext`.
    fn seal(&self, secret: &Secret) -> Result<Vec<u8>, StoreError> {
        let (nonce, ciphertext) = seal(&self.dek, secret.expose())
            .map_err(|_| StoreError::Sqlite("sealing the oidc client secret failed".to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Open a `nonce || ciphertext` blob into the decrypted client secret wrapped in a [`Secret`].
    fn unseal(&self, sealed: &[u8]) -> Result<Secret, StoreError> {
        if sealed.len() < NONCE_LEN {
            return Err(StoreError::Sqlite(
                "stored oidc client secret is truncated".to_string(),
            ));
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let plaintext = open(&self.dek, nonce, ciphertext).map_err(|_| {
            StoreError::Sqlite("decrypting the oidc client secret failed".to_string())
        })?;
        Ok(Secret::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{
        applied_migrations, migrate, Db, FileSource, MemorySource, SystemDb, SYSTEM_MIGRATIONS,
    };

    fn migrated_conn() -> Connection {
        SystemDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection()
    }

    fn sample<'a>(secret: Option<&'a Secret>) -> NewOidcProvider<'a> {
        NewOidcProvider {
            provider: "google",
            issuer: "https://accounts.google.com",
            client_id: "qfs-rp-client",
            client_secret: secret,
            redirect_uri: Some("https://host.example/oidc/callback"),
            scopes: "openid email profile",
            authorization_endpoint: Some("https://accounts.google.com/o/oauth2/v2/auth"),
            token_endpoint: Some("https://oauth2.googleapis.com/token"),
            jwks_uri: Some("https://www.googleapis.com/oauth2/v3/certs"),
            jwks_json: Some(r#"{"keys":[]}"#),
        }
    }

    #[test]
    fn upsert_then_get_round_trips_the_provider_and_decrypts_the_secret() {
        let store =
            SqliteOidcProviderStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        assert!(
            store.get_provider("google").unwrap().is_none(),
            "none initially"
        );

        let secret = Secret::from("upstream-client-secret-xyz");
        store.upsert_provider(&sample(Some(&secret))).unwrap();

        let got = store.get_provider("google").unwrap().expect("a provider");
        assert_eq!(got.issuer, "https://accounts.google.com");
        assert_eq!(got.client_id, "qfs-rp-client");
        assert_eq!(got.scopes, "openid email profile");
        assert_eq!(
            got.client_secret.as_ref().map(Secret::expose),
            Some("upstream-client-secret-xyz".as_bytes())
        );
        assert_eq!(store.list_providers().unwrap(), vec!["google".to_string()]);
    }

    #[test]
    fn the_client_secret_column_does_not_contain_the_plaintext() {
        let store =
            SqliteOidcProviderStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        let canary = "PLAINTEXT-CLIENT-SECRET-CANARY-001";
        store
            .upsert_provider(&sample(Some(&Secret::from(canary))))
            .unwrap();
        let conn = store.lock().unwrap();
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT client_secret_encrypted FROM oidc_providers WHERE provider = 'google'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !stored.windows(canary.len()).any(|w| w == canary.as_bytes()),
            "the plaintext client secret leaked into the stored column"
        );
    }

    #[test]
    fn a_public_client_stores_a_null_secret() {
        let store =
            SqliteOidcProviderStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store.upsert_provider(&sample(None)).unwrap();
        let got = store.get_provider("google").unwrap().unwrap();
        assert!(got.client_secret.is_none(), "no secret for a public client");
        // The stored column is genuinely NULL.
        let conn = store.lock().unwrap();
        let is_null: bool = conn
            .query_row(
                "SELECT client_secret_encrypted IS NULL FROM oidc_providers WHERE provider='google'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(is_null);
    }

    #[test]
    fn upsert_replaces_an_existing_provider() {
        let store =
            SqliteOidcProviderStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store
            .upsert_provider(&sample(Some(&Secret::from("secret-v1"))))
            .unwrap();
        // Re-register the same provider with a rotated secret + a new client id.
        let new_secret = Secret::from("secret-v2");
        let mut updated = sample(Some(&new_secret));
        updated.client_id = "qfs-rp-client-rotated";
        store.upsert_provider(&updated).unwrap();

        let got = store.get_provider("google").unwrap().unwrap();
        assert_eq!(got.client_id, "qfs-rp-client-rotated");
        assert_eq!(
            got.client_secret.as_ref().map(Secret::expose),
            Some("secret-v2".as_bytes())
        );
        // Still exactly one row (upsert, not insert).
        assert_eq!(store.list_providers().unwrap().len(), 1);
    }

    #[test]
    fn data_survives_reopen_with_the_same_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system.db");
        {
            let conn = SystemDb::open(&FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection();
            let store = SqliteOidcProviderStore::open_or_init(conn, &Secret::from("correct horse"))
                .unwrap();
            store
                .upsert_provider(&sample(Some(&Secret::from("persisted-secret"))))
                .unwrap();
        }
        // Reopen with the SAME passphrase: the sealed secret decrypts to the same plaintext.
        let conn = SystemDb::open(&FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        let store =
            SqliteOidcProviderStore::open_or_init(conn, &Secret::from("correct horse")).unwrap();
        let got = store.get_provider("google").unwrap().unwrap();
        assert_eq!(
            got.client_secret.as_ref().map(Secret::expose),
            Some("persisted-secret".as_bytes())
        );
    }

    #[test]
    fn wrong_passphrase_is_locked_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system.db");
        {
            let conn = SystemDb::open(&FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection();
            SqliteOidcProviderStore::open_or_init(conn, &Secret::from("right")).unwrap();
        }
        let conn = SystemDb::open(&FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        match SqliteOidcProviderStore::open_or_init(conn, &Secret::from("wrong")) {
            Err(StoreError::Locked) => {}
            Err(e) => panic!("a wrong passphrase must yield Locked, got {e:?}"),
            Ok(_) => panic!("a wrong passphrase must fail to unlock the oidc provider store"),
        }
    }

    #[test]
    fn system_oidc_providers_migration_v9_applies_idempotently() {
        // t56: migration #9 is idempotent — opening the System DB twice applies it once, re-verifies
        // it the second time (checksum), and the provider registry + its envelope meta exist.
        let mut db = Db::open(&MemorySource).unwrap();
        let applied = migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        assert!(applied.contains(&9), "v9 applied on the first migrate");
        // A relaunch re-applies nothing (the v9 body is re-verified by checksum, not re-run).
        assert!(migrate(&mut db, SYSTEM_MIGRATIONS).unwrap().is_empty());
        // The tables + the issuer index exist.
        let table_exists = |name: &str| -> bool {
            db.conn()
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [name],
                    |_| Ok(true),
                )
                .optional()
                .unwrap()
                .unwrap_or(false)
        };
        assert!(table_exists("oidc_providers"));
        assert!(table_exists("oidc_provider_meta"));
        let idx_exists: bool = db
            .conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='oidc_providers_issuer'",
                [],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .unwrap_or(false);
        assert!(idx_exists, "the issuer index was created");
        // v9 (this migration) is in the applied ledger.
        let ledger = applied_migrations(&db).unwrap();
        assert!(ledger.iter().any(|m| m.version == 9));
    }
}
