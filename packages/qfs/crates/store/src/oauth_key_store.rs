//! t48: the **rusqlite OAuth-signing-key store** over the System DB.
//!
//! This lives in `qfs-store` (not in `qfs-oauth`) on purpose: `qfs-oauth` is a pure-ish leaf with NO
//! rusqlite/tokio, and `qfs-store` is the crate that owns a real `rusqlite::Connection`. So the
//! key-domain logic (keygen, JWK rendering, JWS sign/verify) stays in `qfs-oauth` and its SQLite
//! persistence is here — the same split t43 uses for the secret store (`SqliteSecrets`) and t45/t46
//! use for identity/session. This store trades only in OPAQUE bytes + the PUBLIC JWK JSON string; it
//! never interprets a key (the binary bridges `qfs-oauth` ↔ this store), so `qfs-store` gains no
//! `qfs-oauth` edge.
//!
//! ## Envelope at rest (decision E, mirroring t43's `secret_meta`)
//! On first open the store generates a random System-DB data-key (DEK), derives a key-encryption-key
//! (KEK) from `QFS_PASSPHRASE` + a fresh argon2id salt, wraps the DEK under the KEK, and records the
//! `(wrapped_dek, kdf_salt)` once in `oauth_key_meta`. Each AS PRIVATE signing key is sealed under
//! the DEK (ChaCha20-Poly1305, fresh nonce) into `oauth_keys.private_key_encrypted`. Reopening
//! re-derives the KEK and unwraps the SAME DEK; a wrong passphrase fails to unwrap →
//! [`StoreError::Locked`]. The PUBLIC JWK (`oauth_keys.public_jwk`) is stored in the clear — it is
//! published at `/jwks.json`.
//!
//! ## Secret hygiene (RFD §10)
//! The DEK, the KEK, and the decrypted private scalar are NEVER logged or formatted. A decrypted
//! private key leaves this store ONLY inside a redacting, zeroized [`Secret`]. Every error is
//! secret-free.

use std::sync::Mutex;

use qfs_secrets::{
    derive_kek, generate_dek, generate_salt, open, seal, unwrap_dek, wrap_dek, Secret,
};
use rusqlite::{Connection, OptionalExtension};

use crate::{Db, StoreError};

/// The AEAD nonce width ChaCha20-Poly1305 uses (must match `qfs_secrets::envelope`). The sealed
/// private key is stored as `nonce(12) || ciphertext` in one BLOB column.
const NONCE_LEN: usize = 12;

/// A reloaded AS signing key as it leaves the store: the public material (kid/alg/public_jwk JSON,
/// all clear) plus the DECRYPTED private scalar wrapped in a [`Secret`]. The binary hands the
/// `private_scalar` to `qfs_oauth::SigningKey::from_secret_scalar` to reconstruct the key; it is
/// never rendered.
pub struct StoredSigningKey {
    /// The key id (RFC 7638 thumbprint) — also the `oauth_keys` primary key.
    pub kid: String,
    /// The JWS algorithm — `ES256`.
    pub alg: String,
    /// The PUBLIC JWK JSON, published verbatim at `/jwks.json`.
    pub public_jwk: String,
    /// The DECRYPTED private scalar, redacted + zeroized in memory. Never serialized/logged.
    pub private_scalar: Secret,
}

/// The System-DB-backed OAuth signing-key store. Owns the migrated connection inside a `Mutex` (so
/// the backend is `Send + Sync`) plus the unwrapped System-DB data-key held only in process memory.
/// Never `Debug` (it holds key material).
pub struct OauthKeyStore {
    conn: Mutex<Connection>,
    /// The unwrapped 32-byte System-DB data-key. Seals/opens every private signing key; never raw.
    dek: [u8; 32],
}

impl OauthKeyStore {
    /// Open the store over a migrated System-DB `conn`, unlocking (or initializing) the envelope with
    /// `passphrase`. The OAuth-keys migration (v5) must already be applied — `SystemDb::open` does
    /// that on start.
    ///
    /// - First open (no `oauth_key_meta` row): generate a DEK, derive a KEK from `passphrase` + a
    ///   fresh salt, wrap the DEK, INSERT the single meta row.
    /// - Subsequent opens: read `(wrapped_dek, kdf_salt)`, re-derive the KEK, unwrap the same DEK.
    ///
    /// # Errors
    /// [`StoreError::Locked`] if the passphrase is wrong or the meta row is tampered;
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn open_or_init(conn: Connection, passphrase: &Secret) -> Result<Self, StoreError> {
        let meta: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT wrapped_dek, kdf_salt FROM oauth_key_meta WHERE id = 1",
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
                    StoreError::Sqlite("wrapping the oauth data key failed".to_string())
                })?;
                conn.execute(
                    "INSERT INTO oauth_key_meta (id, wrapped_dek, kdf_salt) VALUES (1, ?1, ?2)",
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
    /// As [`OauthKeyStore::open_or_init`].
    pub fn from_db(db: Db, passphrase: &Secret) -> Result<Self, StoreError> {
        Self::open_or_init(db.into_connection(), passphrase)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Sqlite("oauth key store lock poisoned".to_string()))
    }

    /// The single `active` signing key, or `None` if none has been generated yet. The private scalar
    /// is envelope-decrypted into a [`Secret`]; the public columns are returned as-is.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB / decrypt failure (the DEK is valid — we unwrapped it on open —
    /// so a decrypt failure means a corrupt/tampered column, reported secret-free).
    pub fn active_key(&self) -> Result<Option<StoredSigningKey>, StoreError> {
        let conn = self.lock()?;
        let row: Option<(String, String, String, Vec<u8>)> = conn
            .query_row(
                "SELECT kid, alg, public_jwk, private_key_encrypted FROM oauth_keys \
                 WHERE status = 'active' ORDER BY created_at LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some((kid, alg, public_jwk, sealed)) = row else {
            return Ok(None);
        };
        let private_scalar = self.unseal(&sealed)?;
        Ok(Some(StoredSigningKey {
            kid,
            alg,
            public_jwk,
            private_scalar,
        }))
    }

    /// Insert a freshly generated `active` signing key: seal `private_scalar` under the DEK and store
    /// it alongside the clear public JWK. The caller (the binary) only inserts when [`active_key`]
    /// returned `None`, so the existing active key is reused on a second boot rather than replaced.
    ///
    /// [`active_key`]: OauthKeyStore::active_key
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a seal / DB failure (e.g. a duplicate `kid`).
    pub fn insert_active_key(
        &self,
        kid: &str,
        alg: &str,
        public_jwk: &str,
        private_scalar: &Secret,
    ) -> Result<(), StoreError> {
        let sealed = self.seal(private_scalar)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oauth_keys (kid, alg, public_jwk, private_key_encrypted, status) \
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![kid, alg, public_jwk, sealed],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// The PUBLIC JWK JSON of every published key — the `active` key first, then any `retiring`
    /// keys (the rotation-overlap set). These strings are concatenated into the `/jwks.json` body.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn published_public_jwks(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT public_jwk FROM oauth_keys WHERE status IN ('active','retiring') \
                 ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, created_at DESC",
            )
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

    /// Seal a private scalar under the DEK, returning `nonce || ciphertext`.
    fn seal(&self, private_scalar: &Secret) -> Result<Vec<u8>, StoreError> {
        let (nonce, ciphertext) = seal(&self.dek, private_scalar.expose())
            .map_err(|_| StoreError::Sqlite("sealing the oauth private key failed".to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Open a `nonce || ciphertext` blob into the decrypted scalar wrapped in a [`Secret`].
    fn unseal(&self, sealed: &[u8]) -> Result<Secret, StoreError> {
        if sealed.len() < NONCE_LEN {
            return Err(StoreError::Sqlite(
                "stored oauth private key is truncated".to_string(),
            ));
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let plaintext = open(&self.dek, nonce, ciphertext).map_err(|_| {
            StoreError::Sqlite("decrypting the oauth private key failed".to_string())
        })?;
        Ok(Secret::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{FileSource, MemorySource, SystemDb};

    fn migrated_conn() -> Connection {
        SystemDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection()
    }

    /// A deterministic 32-byte private scalar fixture (the value the binary would generate via
    /// `qfs-oauth`; here we exercise the storage/envelope path with a fixed value).
    const SCALAR: [u8; 32] = [0x11; 32];

    #[test]
    fn insert_then_active_key_round_trips_the_private_scalar() {
        let store = OauthKeyStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        assert!(store.active_key().unwrap().is_none(), "no key initially");

        store
            .insert_active_key(
                "kid-1",
                "ES256",
                r#"{"kty":"EC"}"#,
                &Secret::new(SCALAR.to_vec()),
            )
            .unwrap();

        let active = store.active_key().unwrap().expect("an active key");
        assert_eq!(active.kid, "kid-1");
        assert_eq!(active.alg, "ES256");
        assert_eq!(active.public_jwk, r#"{"kty":"EC"}"#);
        // The decrypted private scalar matches what was sealed.
        assert_eq!(active.private_scalar.expose(), SCALAR);
    }

    #[test]
    fn the_private_key_column_does_not_contain_the_plaintext_scalar() {
        let store = OauthKeyStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        // A recognizable plaintext canary as the "scalar".
        let canary = b"PLAINTEXT-PRIVATE-KEY-CANARY-0011".to_vec();
        store
            .insert_active_key("kid-c", "ES256", "{}", &Secret::new(canary.clone()))
            .unwrap();
        let conn = store.lock().unwrap();
        let stored: Vec<u8> = conn
            .query_row("SELECT private_key_encrypted FROM oauth_keys", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !stored.windows(canary.len()).any(|w| w == canary.as_slice()),
            "the plaintext private key leaked into the stored column"
        );
    }

    #[test]
    fn published_jwks_lists_active_first_then_retiring() {
        let store = OauthKeyStore::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store
            .insert_active_key(
                "kid-active",
                "ES256",
                r#"{"kid":"active"}"#,
                &Secret::new(SCALAR.to_vec()),
            )
            .unwrap();
        // Simulate a retiring key (a rotation overlap) by inserting one directly with status.
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO oauth_keys (kid, alg, public_jwk, private_key_encrypted, status) \
                 VALUES ('kid-old', 'ES256', '{\"kid\":\"old\"}', x'00', 'retiring')",
                [],
            )
            .unwrap();
        }
        let published = store.published_public_jwks().unwrap();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0], r#"{"kid":"active"}"#, "active key first");
        assert_eq!(published[1], r#"{"kid":"old"}"#, "retiring key after");
    }

    #[test]
    fn data_survives_reopen_and_the_key_is_reused() {
        // A file-backed System DB so the DEK + sealed key genuinely persist across reopen — the
        // SECOND-BOOT KEY-REUSE assertion (the binary inserts only when active_key() is None).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system.db");
        let (kid, jwk) = {
            let conn = SystemDb::open(&FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection();
            let store = OauthKeyStore::open_or_init(conn, &Secret::from("correct horse")).unwrap();
            store
                .insert_active_key(
                    "kid-persist",
                    "ES256",
                    r#"{"kty":"EC","kid":"persist"}"#,
                    &Secret::new(SCALAR.to_vec()),
                )
                .unwrap();
            let a = store.active_key().unwrap().unwrap();
            (a.kid, a.public_jwk)
        };
        // Reopen with the SAME passphrase: the SAME active key (kid + public JWK + private scalar) is
        // returned, so the binary's "insert only if absent" reuses it rather than minting a new one.
        let conn = SystemDb::open(&FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        let store = OauthKeyStore::open_or_init(conn, &Secret::from("correct horse")).unwrap();
        let again = store
            .active_key()
            .unwrap()
            .expect("the active key persisted");
        assert_eq!(again.kid, kid, "the active kid is reused on second boot");
        assert_eq!(again.public_jwk, jwk);
        assert_eq!(again.private_scalar.expose(), SCALAR);
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
            OauthKeyStore::open_or_init(conn, &Secret::from("right")).unwrap();
        }
        let conn = SystemDb::open(&FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        // `OauthKeyStore` is intentionally NOT Debug (it holds key material), so match the Result
        // without formatting the Ok value.
        match OauthKeyStore::open_or_init(conn, &Secret::from("wrong")) {
            Err(StoreError::Locked) => {}
            Err(e) => panic!("a wrong passphrase must yield Locked, got {e:?}"),
            Ok(_) => panic!("a wrong passphrase must fail to unlock the oauth key store"),
        }
    }
}
