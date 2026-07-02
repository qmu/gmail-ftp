//! t43: the **SQLite-backed [`Secrets`] backend** — the binary's default at-rest credential
//! store, replacing the old file vault ([`qfs_secrets::LocalStore`]).
//!
//! This lives in the binary (not in `qfs-secrets`) on purpose: the dep-direction guard
//! `secrets_is_confined_to_types_and_core_consumes_it` requires `qfs-secrets` to depend ONLY on
//! `qfs-types` among workspace crates, so it must NOT pull in `qfs-store`/`rusqlite`. The binary is
//! the one place that owns a real DB connection (decision F), so the `Secrets` impl that needs that
//! connection lives here. The **pure** crypto it builds on ([`qfs_secrets::envelope`]) stays in
//! `qfs-secrets`, behind the trait.
//!
//! ## Envelope at rest (roadmap §4.2)
//! On first open the store generates a random 32-byte data-key (DEK), derives a key-encryption-key
//! (KEK) from `QFS_PASSPHRASE` + a fresh argon2id salt, wraps the DEK under the KEK, and records the
//! `(wrapped_dek, kdf_salt)` once in `secret_meta`. Every secret VALUE is sealed under the DEK with a
//! fresh nonce into `secret_store(nonce, ciphertext)`. Reopening re-derives the KEK and unwraps the
//! same DEK, so the data survives reopen with the same passphrase; a wrong passphrase fails to unwrap
//! and the store is [`SecretError::Locked`].
//!
//! ## Secret hygiene (RFD §10)
//! The DEK, the KEK, the `Secret`, and the raw ciphertext are NEVER logged or formatted. Every error
//! is secret-free: a backend failure carries an *operation description*, never key material.

use std::sync::Mutex;

use qfs_secrets::{
    derive_kek, generate_dek, generate_salt, open, rewrap_dek, seal, unlock_via_slots, wrap_dek,
    ConnectionId, ConnectionRecord, CredentialKey, DriverId, OwnerScope, Secret, SecretError,
    Secrets, SlotWrap,
};
use rusqlite::{Connection, OptionalExtension};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The passphrase guardian's slot kind (ADR 0008 §5): the KEK is argon2id-derived from the
/// operator's passphrase over the slot's `kdf_salt`.
pub const GUARDIAN_PASSPHRASE: &str = "passphrase";
/// The OS-keychain guardian's slot kind: a raw random KEK held by the platform secret service
/// (no KDF salt) — non-interactive once enrolled.
pub const GUARDIAN_KEYCHAIN: &str = "keychain";

/// The SQLite-backed credential store. Owns the migrated Project-DB connection inside a `Mutex` (so
/// the whole backend is `Send + Sync` behind `Arc<dyn Secrets>`) plus the unwrapped data-key held
/// only in process memory. Never `Debug` (it holds key material).
pub struct SqliteSecrets {
    /// The migrated Project-DB connection, owned so the backend is self-contained.
    conn: Mutex<Connection>,
    /// The unwrapped 32-byte data-key. Seals/opens every secret value; never persisted raw.
    dek: [u8; 32],
}

impl SqliteSecrets {
    /// Open the store over a migrated Project-DB `conn`, unlocking (or initializing) the envelope
    /// with `passphrase`.
    ///
    /// - First open (no `secret_meta` row): generate a DEK, derive a KEK from `passphrase` + a fresh
    ///   salt, wrap the DEK, and INSERT the single meta row.
    /// - Subsequent opens: read `(wrapped_dek, kdf_salt)`, re-derive the KEK, and unwrap the DEK.
    ///
    /// # Errors
    /// [`SecretError::Locked`] if the passphrase is wrong or the meta row is tampered (the DEK
    /// cannot be unwrapped); [`SecretError::Backend`] on a DB/seal failure (secret-free message).
    pub fn open_or_init(conn: Connection, passphrase: &Secret) -> Result<Self, SecretError> {
        let slots = Self::db_load_slots(&conn)?;
        let dek = if slots.is_empty() {
            // Fresh store: mint a DEK and enroll the passphrase as slot #1 (ADR 0008: the
            // passphrase is the FIRST guardian, not the mechanism — keychain/agent/KMS slots
            // enroll beside it later).
            let dek = generate_dek();
            let salt = generate_salt();
            let kek = derive_kek(passphrase.expose(), &salt).map_err(|_| SecretError::Locked)?;
            let wrapped = wrap_dek(&kek, &dek)
                .map_err(|_| SecretError::Backend("wrapping the data key".into()))?;
            conn.execute(
                "INSERT INTO vault_key_slot (guardian_kind, wrapped_dek, kdf_salt) \
                 VALUES ('passphrase', ?1, ?2)",
                rusqlite::params![wrapped, salt.as_slice()],
            )
            .map_err(|e| SecretError::Backend(format!("initializing the vault key slot: {e}")))?;
            dek
        } else {
            // Established store: the passphrase guardian tries each passphrase slot (a wrong
            // passphrase / tampered wrap fails authentication -> Locked, slot-anonymous).
            unlock_via_slots(&slots, |slot| {
                if slot.guardian_kind != GUARDIAN_PASSPHRASE {
                    return None;
                }
                let salt = slot.kdf_salt.as_deref()?;
                derive_kek(passphrase.expose(), salt).ok()
            })
            .map_err(|_| SecretError::Locked)?
        };

        Ok(Self {
            conn: Mutex::new(conn),
            dek,
        })
    }

    /// Open an ESTABLISHED store through an arbitrary guardian resolver (ADR 0008 §5): the binary
    /// composes the available guardians (an enrolled OS-keychain KEK, a cached passphrase, …) into
    /// `kek_of`; the first slot that opens yields the DEK. Never initializes — a fresh store has
    /// nothing to unlock (use [`Self::open_or_init`]).
    ///
    /// # Errors
    /// [`SecretError::Locked`] when no slot opens (no guardian available, wrong key material, or an
    /// empty slot set — deliberately indistinguishable); [`SecretError::Backend`] on a DB failure.
    pub fn open_with_resolver<F>(conn: Connection, kek_of: F) -> Result<Self, SecretError>
    where
        F: FnMut(&SlotWrap) -> Option<[u8; 32]>,
    {
        let slots = Self::db_load_slots(&conn)?;
        let dek = unlock_via_slots(&slots, kek_of).map_err(|_| SecretError::Locked)?;
        Ok(Self {
            conn: Mutex::new(conn),
            dek,
        })
    }

    /// Load the vault-key slots (passphrase-free: wraps + public metadata only). Ordered
    /// keychain-first so a non-interactive guardian gets its chance before anything that would
    /// prompt, then by slot id (stable).
    ///
    /// # Errors
    /// [`SecretError::Backend`] on a DB failure.
    pub fn db_load_slots(conn: &Connection) -> Result<Vec<SlotWrap>, SecretError> {
        let mut stmt = conn
            .prepare(
                "SELECT slot_id, guardian_kind, wrapped_dek, kdf_salt FROM vault_key_slot \
                 ORDER BY (guardian_kind = 'keychain') DESC, slot_id",
            )
            .map_err(|e| SecretError::Backend(format!("reading vault key slots: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SlotWrap {
                    slot_id: r.get(0)?,
                    guardian_kind: r.get(1)?,
                    wrapped_dek: r.get(2)?,
                    kdf_salt: r.get(3)?,
                })
            })
            .map_err(|e| SecretError::Backend(format!("reading vault key slots: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SecretError::Backend(format!("reading vault key slots: {e}")))?;
        Ok(rows)
    }

    /// **Enroll a new vault-key slot** (ADR 0008 §5): wrap this store's (already unlocked) DEK
    /// under `kek` and insert the slot row. No sealed value is touched — that is the point of the
    /// slot model. Returns the new slot id.
    ///
    /// # Errors
    /// [`SecretError::Backend`] on a wrap/DB failure (secret-free message).
    pub fn enroll_slot(
        &self,
        guardian_kind: &str,
        kek: &[u8; 32],
        kdf_salt: Option<&[u8]>,
    ) -> Result<i64, SecretError> {
        let wrapped = wrap_dek(kek, &self.dek)
            .map_err(|_| SecretError::Backend("wrapping the data key for the new slot".into()))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO vault_key_slot (guardian_kind, wrapped_dek, kdf_salt) VALUES (?1, ?2, ?3)",
            rusqlite::params![guardian_kind, wrapped, kdf_salt],
        )
        .map_err(|e| SecretError::Backend(format!("enrolling the vault key slot: {e}")))?;
        Ok(conn.last_insert_rowid())
    }

    /// **Revoke a vault-key slot** — delete one wrap. The LAST slot is refused (a store with no
    /// slot could never be opened again; delete the store, don't brick it).
    ///
    /// # Errors
    /// [`SecretError::Backend`] when the slot does not exist, it is the last one, or on a DB
    /// failure.
    pub fn revoke_slot(&self, slot_id: i64) -> Result<(), SecretError> {
        let conn = self.lock()?;
        // Guard in one statement: the row is deleted only while ANOTHER slot remains.
        let n = conn
            .execute(
                "DELETE FROM vault_key_slot WHERE slot_id = ?1 \
                 AND (SELECT COUNT(*) FROM vault_key_slot) > 1",
                rusqlite::params![slot_id],
            )
            .map_err(|e| SecretError::Backend(format!("revoking the vault key slot: {e}")))?;
        if n == 0 {
            return Err(SecretError::Backend(
                "slot not revoked: it does not exist, or it is the last slot (a store needs at \
                 least one guardian — enroll another before revoking this one)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// List the vault-key slots as display metadata `(slot_id, guardian_kind, created_at)` — never
    /// wrap material.
    ///
    /// # Errors
    /// [`SecretError::Backend`] on a DB failure.
    pub fn list_slots(&self) -> Result<Vec<(i64, String, String)>, SecretError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT slot_id, guardian_kind, created_at FROM vault_key_slot ORDER BY slot_id",
            )
            .map_err(|e| SecretError::Backend(format!("listing vault key slots: {e}")))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| SecretError::Backend(format!("listing vault key slots: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SecretError::Backend(format!("listing vault key slots: {e}")))?;
        Ok(rows)
    }

    /// Consume the store and yield its owned (migrated) Project-DB connection — for flows that
    /// unlock, mutate slots, and then re-open through a DIFFERENT guardian (and for tests). The
    /// in-memory DEK is dropped with the store.
    #[must_use]
    pub fn into_connection(self) -> Connection {
        self.conn
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock the connection mutex, mapping a poisoned lock to a secret-free backend error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SecretError> {
        self.conn
            .lock()
            .map_err(|_| SecretError::Backend("secret store lock poisoned".into()))
    }

    /// **Rotate (re-mint)** the secret for `key` (t79, §4.5): re-seal a NEW credential value under
    /// the SAME data-key, stamp `last_rotated`, and CLEAR any revocation. This is the offboarding /
    /// compromise answer — the secret the departing member could have copied is *replaced*, so the
    /// old value stops working while the connection keeps working for the team under the new value.
    ///
    /// Atomic: a single UPSERT swaps the sealed value, sets `last_rotated = now`, and resets
    /// `revoked_at = NULL` in one statement. The new value arrives via the credential-input path
    /// (stdin), never a query literal (§4.5); it is consumed into the seal and never logged.
    ///
    /// # Errors
    /// [`SecretError::Backend`] on a seal/DB failure (secret-free message).
    pub fn rotate(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError> {
        let conn = self.lock()?;
        let (nonce, ciphertext) = seal(&self.dek, value.expose())
            .map_err(|_| SecretError::Backend("sealing credential".into()))?;
        conn.execute(
            "INSERT INTO secret_store (driver, connection, nonce, ciphertext, last_rotated, revoked_at) \
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ','now'), NULL) \
             ON CONFLICT(driver, connection) DO UPDATE SET \
                 nonce = excluded.nonce, \
                 ciphertext = excluded.ciphertext, \
                 last_rotated = strftime('%Y-%m-%dT%H:%M:%SZ','now'), \
                 revoked_at = NULL",
            rusqlite::params![
                key.driver.as_str(),
                key.connection.as_str(),
                nonce.as_slice(),
                ciphertext
            ],
        )
        .map_err(|e| SecretError::Backend(format!("rotating credential: {e}")))?;
        Ok(())
    }

    /// **Revoke** the credential for `key` (t79): mark the connection unresolvable by stamping
    /// `revoked_at`. After this, [`Secrets::get`] refuses the connection with
    /// [`SecretError::Revoked`] — the secret is never decrypted or returned (default-deny). The
    /// ciphertext is left in place (a later `rotate` re-mints + clears the mark); revocation changes
    /// WHETHER the secret resolves, never the at-rest crypto. Selectors only — no secret touched.
    ///
    /// # Errors
    /// [`SecretError::NotFound`] if no credential exists for `key` (there is nothing to revoke);
    /// [`SecretError::Backend`] on a DB failure (secret-free message).
    pub fn revoke(&self, key: &CredentialKey) -> Result<(), SecretError> {
        let conn = self.lock()?;
        let affected = conn
            .execute(
                "UPDATE secret_store SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
                 WHERE driver = ?1 AND connection = ?2",
                rusqlite::params![key.driver.as_str(), key.connection.as_str()],
            )
            .map_err(|e| SecretError::Backend(format!("revoking credential: {e}")))?;
        if affected == 0 {
            return Err(SecretError::NotFound(key.clone()));
        }
        Ok(())
    }

    /// **DEK re-wrap on a passphrase change** (t79, §4.2): rotate the key-encryption-key WITHOUT
    /// re-encrypting a single secret column. Unwrap the data-key under the KEK derived from
    /// `old_pass`, then re-wrap the SAME DEK under a KEK derived from `new_pass` + a fresh salt, and
    /// persist `(wrapped_dek, kdf_salt)` in the single `secret_meta` row. Because the DEK is
    /// unchanged, every existing secret still decrypts; because the salt + wrapped-DEK change, the
    /// OLD passphrase no longer unwraps the store on the next open.
    ///
    /// A **wrong** `old_pass` fails to unwrap and returns [`SecretError::Locked`] BEFORE any write —
    /// there is no silent re-key under a wrong old passphrase. No DEK/KEK/passphrase is ever logged.
    ///
    /// # Errors
    /// [`SecretError::Locked`] if `old_pass` cannot unwrap the stored DEK (wrong passphrase /
    /// tampered metadata) or the store has no metadata row; [`SecretError::Backend`] on a DB failure.
    pub fn rewrap_passphrase(
        &self,
        old_pass: &Secret,
        new_pass: &Secret,
    ) -> Result<(), SecretError> {
        let conn = self.lock()?;
        // The passphrase slot(s) (ADR 0008 §5: rekey is SLOT-SCOPED — other guardians' wraps are
        // untouched, so an enrolled keychain keeps unlocking across a passphrase change).
        let mut stmt = conn
            .prepare(
                "SELECT slot_id, wrapped_dek, kdf_salt FROM vault_key_slot \
                 WHERE guardian_kind = 'passphrase' ORDER BY slot_id",
            )
            .map_err(|e| SecretError::Backend(format!("reading the passphrase slot: {e}")))?;
        let slots: Vec<(i64, Vec<u8>, Option<Vec<u8>>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| SecretError::Backend(format!("reading the passphrase slot: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SecretError::Backend(format!("reading the passphrase slot: {e}")))?;
        drop(stmt);
        if slots.is_empty() {
            return Err(SecretError::Locked);
        }

        // Find the passphrase slot the OLD passphrase opens; re-wrap the SAME DEK under a NEW KEK
        // (fresh salt) and update THAT row. `rewrap_dek` unwraps under the old KEK first, so a
        // wrong old passphrase fails authentication -> Locked BEFORE any write.
        for (slot_id, wrapped, salt) in &slots {
            let Some(salt) = salt.as_deref() else {
                continue;
            };
            let Ok(old_kek) = derive_kek(old_pass.expose(), salt) else {
                continue;
            };
            let new_salt = generate_salt();
            let new_kek =
                derive_kek(new_pass.expose(), &new_salt).map_err(|_| SecretError::Locked)?;
            if let Ok(new_wrapped) = rewrap_dek(&old_kek, &new_kek, wrapped) {
                conn.execute(
                    "UPDATE vault_key_slot SET wrapped_dek = ?1, kdf_salt = ?2 WHERE slot_id = ?3",
                    rusqlite::params![new_wrapped, new_salt.as_slice(), slot_id],
                )
                .map_err(|e| SecretError::Backend(format!("re-wrapping the data key: {e}")))?;
                return Ok(());
            }
        }
        Err(SecretError::Locked)
    }
}

impl Secrets for SqliteSecrets {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        let conn = self.lock()?;
        // t79: SELECT `revoked_at` alongside the sealed value so REVOCATION is enforced BEFORE any
        // decrypt. A revoked connection short-circuits to `SecretError::Revoked` — the DEK is never
        // applied and the secret is NEVER returned (default-deny on offboarding / compromise).
        let row: Option<(Vec<u8>, Vec<u8>, Option<String>)> = conn
            .query_row(
                "SELECT nonce, ciphertext, revoked_at FROM secret_store \
                 WHERE driver = ?1 AND connection = ?2",
                rusqlite::params![key.driver.as_str(), key.connection.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| SecretError::Backend(format!("reading credential: {e}")))?;
        match row {
            // Revoked: refuse to resolve. We never touch the ciphertext — the secret cannot leak.
            Some((_, _, Some(_revoked_at))) => Err(SecretError::Revoked(key.clone())),
            Some((nonce, ciphertext, None)) => {
                // Decrypt straight into a Secret; a failed open is a backend error (the DEK is
                // valid — we unwrapped it on open — so this means a corrupt/tampered column).
                let plaintext = open(&self.dek, &nonce, &ciphertext)
                    .map_err(|_| SecretError::Backend("decrypting credential".into()))?;
                Ok(Secret::new(plaintext))
            }
            None => Err(SecretError::NotFound(key.clone())),
        }
    }

    fn put(&self, key: &CredentialKey, value: Secret) -> Result<(), SecretError> {
        let conn = self.lock()?;
        let (nonce, ciphertext) = seal(&self.dek, value.expose())
            .map_err(|_| SecretError::Backend("sealing credential".into()))?;
        conn.execute(
            "INSERT INTO secret_store (driver, connection, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(driver, connection) DO UPDATE SET \
                 nonce = excluded.nonce, \
                 ciphertext = excluded.ciphertext, \
                 created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
            rusqlite::params![key.driver.as_str(), key.connection.as_str(), nonce.as_slice(), ciphertext],
        )
        .map_err(|e| SecretError::Backend(format!("storing credential: {e}")))?;
        Ok(())
    }

    fn remove(&self, key: &CredentialKey) -> Result<(), SecretError> {
        let conn = self.lock()?;
        // Idempotent: deleting an absent key affects zero rows and is still Ok.
        conn.execute(
            "DELETE FROM secret_store WHERE driver = ?1 AND connection = ?2",
            rusqlite::params![key.driver.as_str(), key.connection.as_str()],
        )
        .map_err(|e| SecretError::Backend(format!("removing credential: {e}")))?;
        Ok(())
    }

    fn list(&self, driver: Option<&DriverId>) -> Result<Vec<ConnectionRecord>, SecretError> {
        let conn = self.lock()?;
        // t81: LEFT JOIN the `shared_connection` registry so each listed connection carries its
        // OWNER (`me` vs `project`) — a connection with a `shared_connection` row is project/team
        // owned. SELECTORS + metadata only (no `nonce`/`ciphertext`): the redaction contract holds —
        // the listing never touches the encrypted value.
        let mut stmt = conn
            .prepare(
                "SELECT s.driver, s.connection, s.created_at, sc.driver AS shared \
                 FROM secret_store s \
                 LEFT JOIN shared_connection sc \
                   ON sc.driver = s.driver AND sc.connection = s.connection \
                 ORDER BY s.driver, s.connection",
            )
            .map_err(|e| SecretError::Backend(format!("listing connections: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    // The joined `shared_connection.driver` is non-NULL iff the connection is shared.
                    r.get::<_, Option<String>>(3)?.is_some(),
                ))
            })
            .map_err(|e| SecretError::Backend(format!("listing connections: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let (drv, acct, created, shared) =
                row.map_err(|e| SecretError::Backend(format!("listing connections: {e}")))?;
            // A row whose connection name no longer parses is skipped rather than failing the list
            // (mirrors LocalStore::list); the names were validated on `put`, so this is defensive.
            let Ok(connection) = ConnectionId::new(acct) else {
                continue;
            };
            let created_at = parse_created_at(&created);
            let owner = if shared {
                OwnerScope::Project
            } else {
                OwnerScope::Me
            };
            let rec = ConnectionRecord::new(DriverId::new(drv), connection, created_at)
                .with_owner_scope(owner);
            if driver.is_none_or(|d| &rec.driver == d) {
                out.push(rec);
            }
        }
        Ok(out)
    }
}

/// Parse a `created_at` column (RFC 3339, e.g. `2026-06-28T10:00:00Z`) back to an
/// [`OffsetDateTime`]. A malformed stamp falls back to the Unix epoch rather than failing the list —
/// the timestamp is display metadata, not load-bearing.
fn parse_created_at(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// A recorded cloud-connection consent grant — selectors + metadata ONLY (subject, scope, time),
/// **never** a secret. This is what [`db_get_consent`] reads back so the commit-time bind gate can
/// confirm a signed-in operator granted this `(driver, connection)` explicit consent (t54 / M4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRow {
    /// The identity (email / user label, t45) that granted consent. Metadata, never a credential.
    pub subject: String,
    /// The OAuth scope the consent was granted for (a §10 hint, never a token).
    pub scope: String,
    /// When consent was granted (RFC 3339).
    pub granted_at: String,
}

/// Record (UPSERT) that the `subject` granted consent for the cloud `driver`/`connection` with
/// `scope`. Last-writer-wins per `(driver, connection)`. Selectors + metadata only — the refresh
/// token itself is sealed separately in `secret_store`; this row carries no key material, so it needs
/// no passphrase (the same passphrase-free path as `connection_consent`).
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free message).
pub fn db_record_consent(
    conn: &Connection,
    driver: &str,
    connection: &str,
    subject: &str,
    scope: &str,
) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO connection_consent (driver, connection, subject, scope) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(driver, connection) DO UPDATE SET \
             subject = excluded.subject, \
             scope = excluded.scope, \
             granted_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![driver, connection, subject, scope],
    )
    .map_err(|e| SecretError::Backend(format!("recording consent: {e}")))?;
    Ok(())
}

/// Read the recorded consent for `driver`/`connection`, or `None` if no consent was granted /
/// unreadable. Best-effort (selectors + metadata only; no passphrase) so the commit resolver can
/// consult it on the passphrase-free path.
#[must_use]
pub fn db_get_consent(conn: &Connection, driver: &str, connection: &str) -> Option<ConsentRow> {
    conn.query_row(
        "SELECT subject, scope, granted_at FROM connection_consent WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
        |r| {
            Ok(ConsentRow {
                subject: r.get(0)?,
                scope: r.get(1)?,
                granted_at: r.get(2)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// A recorded project/team-owned (shared) connection — selectors + metadata ONLY (`scope`, who
/// shared it, when), **never** a secret. The presence of a row marks a connection PROJECT-owned
/// (t81 / decision U / §3.3); its [`scope`](SharedConnectionRow::scope) is the realm path the acting
/// member's actor-policy must grant before the commit-time bind resolves the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedConnectionRow {
    /// The realm path glob (t71, e.g. `/projects/acme/**`) the member's actor-policy must grant to
    /// USE this connection. A §10 hint, never a token.
    pub scope: String,
    /// The identity (email / user label, t45) that shared the connection. Audit metadata for the
    /// §3.3 two-layer trace; never a credential.
    pub shared_by: String,
    /// When the connection was shared (RFC 3339).
    pub created_at: String,
}

/// Mark `driver`/`connection` as PROJECT/TEAM-owned (shared) with the realm `scope` the acting
/// member's actor-policy must grant to USE it, recording `shared_by` (who shared it). UPSERT —
/// re-sharing updates the scope/sharer (last-writer-wins per `(driver, connection)`). Selectors +
/// metadata only — the credential itself stays ENCRYPTED in `secret_store`; this row carries no key
/// material, so it needs no passphrase (the same passphrase-free path as `connection_consent`).
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free message).
pub fn db_share_connection(
    conn: &Connection,
    driver: &str,
    connection: &str,
    scope: &str,
    shared_by: &str,
) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO shared_connection (driver, connection, scope, shared_by) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(driver, connection) DO UPDATE SET \
             scope = excluded.scope, \
             shared_by = excluded.shared_by, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![driver, connection, scope, shared_by],
    )
    .map_err(|e| SecretError::Backend(format!("sharing connection: {e}")))?;
    Ok(())
}

/// Stop sharing `driver`/`connection` — revert it to user-owned by deleting its
/// `shared_connection` row. Idempotent: removing an unshared connection affects zero rows and is
/// still `Ok`. Selectors only; passphrase-free.
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free message).
pub fn db_unshare_connection(
    conn: &Connection,
    driver: &str,
    connection: &str,
) -> Result<(), SecretError> {
    conn.execute(
        "DELETE FROM shared_connection WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
    )
    .map_err(|e| SecretError::Backend(format!("unsharing connection: {e}")))?;
    Ok(())
}

/// Read the project-ownership row for `driver`/`connection`, or `None` if it is user-owned /
/// unreadable. Best-effort + passphrase-free (the row carries no key material); an unreadable
/// Project DB reads as user-owned. The commit-time bind consults this BEFORE any decrypt to decide
/// whether the actor-policy gate applies (a `Some` ⇒ project-owned ⇒ gate; `None` ⇒ ungated).
#[must_use]
pub fn db_get_shared_connection(
    conn: &Connection,
    driver: &str,
    connection: &str,
) -> Option<SharedConnectionRow> {
    conn.query_row(
        "SELECT scope, shared_by, created_at FROM shared_connection \
         WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
        |r| {
            Ok(SharedConnectionRow {
                scope: r.get(0)?,
                shared_by: r.get(1)?,
                created_at: r.get(2)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// List every project/team-owned (shared) connection as `(driver, connection, row)` — selectors +
/// metadata only, never a secret. Powers a `qfs account list --project` / `/sys` style surface that
/// shows which connections are team-shared and at what scope. Best-effort: a query failure yields an
/// empty list rather than erroring (the metadata view never blocks).
#[must_use]
pub fn db_list_shared_connections(conn: &Connection) -> Vec<(String, String, SharedConnectionRow)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT driver, connection, scope, shared_by, created_at FROM shared_connection \
         ORDER BY driver, connection",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            SharedConnectionRow {
                scope: r.get(2)?,
                shared_by: r.get(3)?,
                created_at: r.get(4)?,
            },
        ))
    }) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}

/// A recorded BROKERED team-connection (t66 / M9) — selectors + metadata ONLY (the team it is scoped
/// to, the upstream provider, the broker's PUBLIC client id, the scope, who provisioned it), **never**
/// a secret. The brokered TOKEN stays sealed in `secret_store`; the broker CLIENT SECRET never reaches
/// the tenant DB. This row records the brokering provenance the §3.2 `/sys/connections` view surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConnectionRow {
    /// The team the brokered token is scoped to (the load-bearing binding). Metadata, never a token.
    pub team: String,
    /// The upstream provider key the broker holds a client for (e.g. `google`, `github`, `slack`).
    pub provider: String,
    /// The broker's PUBLIC OAuth client id (qfs Cloud's registered client) — NOT the client secret.
    pub broker_client_id: String,
    /// The upstream scope the brokered token carries (a §10 hint, never a token).
    pub scope: String,
    /// The identity (email / federated handle, t45/t56) that provisioned the team connection. Audit
    /// metadata for the §3.3 two-layer trace; never a credential.
    pub brokered_by: String,
    /// When the team connection was provisioned (RFC 3339).
    pub created_at: String,
}

/// Record (UPSERT) the brokering metadata for a `driver`/`connection` provisioned through the broker
/// (t66 / M9): the `team` it is scoped to, the upstream `provider`, the broker's PUBLIC
/// `broker_client_id`, the `scope`, and who provisioned it (`brokered_by`). Last-writer-wins per
/// `(driver, connection)`. Selectors + metadata only — the brokered token itself is sealed separately
/// in `secret_store`; this row carries no key material, so it needs no passphrase (the same
/// passphrase-free path as `shared_connection`/`connection_consent`).
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free message).
#[allow(clippy::too_many_arguments)]
pub fn db_record_broker_connection(
    conn: &Connection,
    driver: &str,
    connection: &str,
    team: &str,
    provider: &str,
    broker_client_id: &str,
    scope: &str,
    brokered_by: &str,
) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO broker_connection \
             (driver, connection, team, provider, broker_client_id, scope, brokered_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(driver, connection) DO UPDATE SET \
             team = excluded.team, \
             provider = excluded.provider, \
             broker_client_id = excluded.broker_client_id, \
             scope = excluded.scope, \
             brokered_by = excluded.brokered_by, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![
            driver,
            connection,
            team,
            provider,
            broker_client_id,
            scope,
            brokered_by
        ],
    )
    .map_err(|e| SecretError::Backend(format!("recording brokered connection: {e}")))?;
    Ok(())
}

/// Read the brokering metadata for `driver`/`connection`, or `None` if it was not provisioned through
/// the broker / unreadable. Best-effort + passphrase-free (the row carries no key material). The
/// commit-time bind consults this BEFORE any decrypt to learn which team a brokered connection is
/// scoped to (`qfs_oauth::assert_team_scope`).
#[must_use]
pub fn db_get_broker_connection(
    conn: &Connection,
    driver: &str,
    connection: &str,
) -> Option<BrokerConnectionRow> {
    conn.query_row(
        "SELECT team, provider, broker_client_id, scope, brokered_by, created_at \
         FROM broker_connection WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
        |r| {
            Ok(BrokerConnectionRow {
                team: r.get(0)?,
                provider: r.get(1)?,
                broker_client_id: r.get(2)?,
                scope: r.get(3)?,
                brokered_by: r.get(4)?,
                created_at: r.get(5)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_store::{MemorySource, ProjectDb};

    fn migrated_conn() -> Connection {
        ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection()
    }

    fn ckey(driver: &str, connection: &str) -> CredentialKey {
        CredentialKey::new(
            DriverId::new(driver),
            ConnectionId::new(connection).unwrap(),
        )
    }

    #[test]
    fn put_get_remove_round_trip() {
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        let k = ckey("mail", "work");
        assert_eq!(store.get(&k).unwrap_err().code(), "secret_not_found");

        store.put(&k, Secret::from("real-token-xyz")).unwrap();
        assert_eq!(store.get(&k).unwrap().expose_str(), Some("real-token-xyz"));

        store.remove(&k).unwrap();
        assert_eq!(store.get(&k).unwrap_err().code(), "secret_not_found");
        // Remove of an absent key is idempotent.
        store.remove(&k).unwrap();
    }

    #[test]
    fn ciphertext_column_does_not_contain_the_plaintext() {
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store
            .put(
                &ckey("github", "main"),
                Secret::from("ghp_PLAINTEXT_LEAK_CANARY"),
            )
            .unwrap();
        let conn = store.lock().unwrap();
        let ct: Vec<u8> = conn
            .query_row("SELECT ciphertext FROM secret_store", [], |r| r.get(0))
            .unwrap();
        assert!(
            !ct.windows("ghp_PLAINTEXT_LEAK_CANARY".len())
                .any(|w| w == b"ghp_PLAINTEXT_LEAK_CANARY"),
            "plaintext leaked into the ciphertext column"
        );
    }

    #[test]
    fn list_filters_by_driver() {
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store.put(&ckey("mail", "work"), Secret::from("a")).unwrap();
        store.put(&ckey("mail", "home"), Secret::from("b")).unwrap();
        store.put(&ckey("s3", "prod"), Secret::from("c")).unwrap();

        assert_eq!(store.list(None).unwrap().len(), 3);
        assert_eq!(store.list(Some(&DriverId::new("mail"))).unwrap().len(), 2);
    }

    #[test]
    fn data_survives_reopen_with_the_same_passphrase() {
        // A file-backed Project DB so the DEK + ciphertext genuinely persist across reopen.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        {
            let conn = ProjectDb::open(&qfs_store::FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection();
            let store = SqliteSecrets::open_or_init(conn, &Secret::from("correct horse")).unwrap();
            store
                .put(&ckey("gh", "main"), Secret::from("ghp_persisted"))
                .unwrap();
        }
        // Reopen with the SAME passphrase: the DEK unwraps and the value decrypts.
        let conn = ProjectDb::open(&qfs_store::FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("correct horse")).unwrap();
        assert_eq!(
            store.get(&ckey("gh", "main")).unwrap().expose_str(),
            Some("ghp_persisted")
        );
    }

    #[test]
    fn wrong_passphrase_is_locked_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        {
            let conn = ProjectDb::open(&qfs_store::FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection();
            SqliteSecrets::open_or_init(conn, &Secret::from("right")).unwrap();
        }
        // A different passphrase derives a different KEK -> the DEK cannot be unwrapped -> Locked.
        let conn = ProjectDb::open(&qfs_store::FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        // `SqliteSecrets` is intentionally NOT Debug (it holds key material), so match the Result
        // rather than `unwrap_err` (which would require the Ok type to be Debug).
        match SqliteSecrets::open_or_init(conn, &Secret::from("wrong")) {
            Err(err) => assert_eq!(err.code(), "secret_locked"),
            Ok(_) => panic!("a wrong passphrase must fail to unwrap the data key"),
        }
    }

    #[test]
    fn rotate_re_mints_the_secret_atomically() {
        // t79: rotate replaces the stored value under the same DEK — the prior secret no longer
        // resolves, the new one does. Re-mint is the offboarding answer (replace, not un-grant).
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        let k = ckey("github", "team");
        store.put(&k, Secret::from("ghp_old")).unwrap();
        assert_eq!(store.get(&k).unwrap().expose_str(), Some("ghp_old"));

        store.rotate(&k, Secret::from("ghp_new")).unwrap();
        assert_eq!(
            store.get(&k).unwrap().expose_str(),
            Some("ghp_new"),
            "rotate installs the new secret"
        );
        // `last_rotated` is stamped on rotation (plaintext metadata).
        let conn = store.lock().unwrap();
        let rotated: Option<String> = conn
            .query_row(
                "SELECT last_rotated FROM secret_store WHERE driver = 'github' AND connection = 'team'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rotated.is_some(), "rotate stamps last_rotated");
    }

    #[test]
    fn revoke_makes_a_connection_unresolvable_and_never_returns_the_secret() {
        // t79: after revoke, the bind (get) fails closed with a clear `Revoked` error and the
        // secret is NEVER returned. Other connections keep working (revoke is per-connection).
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        let revoked = ckey("github", "leaver");
        let kept = ckey("github", "team");
        store
            .put(&revoked, Secret::from("ghp_LEAKED_CANARY"))
            .unwrap();
        store.put(&kept, Secret::from("ghp_team")).unwrap();

        store.revoke(&revoked).unwrap();
        let err = store.get(&revoked).unwrap_err();
        assert_eq!(err.code(), "secret_revoked", "a revoked bind fails closed");
        // The refusal carries the selectors, never the secret value.
        assert!(!format!("{err:?} {err}").contains("ghp_LEAKED_CANARY"));

        // A different connection is unaffected — revoke is scoped to the one (driver, connection).
        assert_eq!(store.get(&kept).unwrap().expose_str(), Some("ghp_team"));

        // Revoking an absent connection is a clear NotFound (nothing to revoke).
        assert_eq!(
            store.revoke(&ckey("github", "ghost")).unwrap_err().code(),
            "secret_not_found"
        );

        // Re-minting the revoked connection CLEARS the revocation and restores use.
        store
            .rotate(&revoked, Secret::from("ghp_reissued"))
            .unwrap();
        assert_eq!(
            store.get(&revoked).unwrap().expose_str(),
            Some("ghp_reissued"),
            "rotate clears the revocation"
        );
    }

    #[test]
    fn dek_rewrap_under_a_new_passphrase_keeps_secrets_and_locks_out_the_old() {
        // t79: a DEK re-wrap on a passphrase change re-wraps the wrapped-DEK only — existing secrets
        // still decrypt under the new passphrase, while the old passphrase no longer unlocks.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        let open = || {
            ProjectDb::open(&qfs_store::FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection()
        };
        {
            let store = SqliteSecrets::open_or_init(open(), &Secret::from("old-pass")).unwrap();
            store
                .put(&ckey("gh", "main"), Secret::from("ghp_persisted"))
                .unwrap();
            // Re-wrap the DEK from the old passphrase to a new one (no re-seal of the value).
            store
                .rewrap_passphrase(&Secret::from("old-pass"), &Secret::from("new-pass"))
                .unwrap();
            // The SAME open store still decrypts the value (the DEK is unchanged).
            assert_eq!(
                store.get(&ckey("gh", "main")).unwrap().expose_str(),
                Some("ghp_persisted")
            );
        }
        // Reopen with the NEW passphrase: the DEK unwraps and the value decrypts.
        let store = SqliteSecrets::open_or_init(open(), &Secret::from("new-pass")).unwrap();
        assert_eq!(
            store.get(&ckey("gh", "main")).unwrap().expose_str(),
            Some("ghp_persisted")
        );
        // Reopen with the OLD passphrase now FAILS — the re-wrap rotated the KEK.
        match SqliteSecrets::open_or_init(open(), &Secret::from("old-pass")) {
            Err(e) => assert_eq!(e.code(), "secret_locked"),
            Ok(_) => panic!("the old passphrase must no longer unlock after a re-wrap"),
        }
    }

    #[test]
    fn dek_rewrap_under_a_wrong_old_passphrase_is_refused() {
        // t79 non-negotiable: a wrong old passphrase must NOT silently re-key the store.
        let store =
            SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("right-old")).unwrap();
        store
            .put(&ckey("gh", "main"), Secret::from("ghp_x"))
            .unwrap();
        match store.rewrap_passphrase(&Secret::from("WRONG-old"), &Secret::from("new")) {
            Err(e) => assert_eq!(e.code(), "secret_locked"),
            Ok(()) => panic!("a wrong old passphrase must refuse to re-wrap the DEK"),
        }
        // The store still decrypts under the unchanged DEK (no torn re-key happened).
        assert_eq!(
            store.get(&ckey("gh", "main")).unwrap().expose_str(),
            Some("ghp_x")
        );
    }

    #[test]
    fn consent_is_recorded_against_the_connection_and_carries_no_secret() {
        // t54 / M4: granting consent records a row against the (driver, connection) — selectors +
        // metadata only. No passphrase is needed (the row holds no key material), and the recorded
        // value is the consent fact (subject + scope + time), never a credential.
        let conn = migrated_conn();
        assert!(db_get_consent(&conn, "gmail", "work").is_none());

        db_record_consent(&conn, "gmail", "work", "a@b.com", "gmail.readonly").unwrap();
        let row = db_get_consent(&conn, "gmail", "work").expect("consent recorded");
        assert_eq!(row.subject, "a@b.com");
        assert_eq!(row.scope, "gmail.readonly");
        assert!(!row.granted_at.is_empty());

        // The consent ledger is independent per connection and per driver.
        assert!(db_get_consent(&conn, "gmail", "personal").is_none());
        assert!(db_get_consent(&conn, "github", "work").is_none());

        // Last-writer-wins on re-consent (e.g. a re-grant with a wider scope).
        db_record_consent(&conn, "gmail", "work", "a@b.com", "gmail.modify").unwrap();
        assert_eq!(
            db_get_consent(&conn, "gmail", "work").unwrap().scope,
            "gmail.modify"
        );

        // The consent table stores NO credential material — only the metadata columns exist.
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info('connection_consent')")
                .unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert!(
            !cols.iter().any(|c| c.contains("secret")
                || c.contains("token")
                || c.contains("ciphertext")
                || c.contains("nonce")),
            "the consent ledger must carry no secret column, got {cols:?}"
        );
    }

    /// ADR 0008 (migration #11): the `active_account` selection table is GONE — the mount carries
    /// the account, so a migrated store has nothing to select against.
    #[test]
    fn active_account_table_is_dropped() {
        let conn = migrated_conn();
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'active_account'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(exists.is_none(), "active_account must be dropped by v11");
    }

    #[test]
    fn sharing_marks_a_connection_project_owned_and_carries_no_secret() {
        // t81: sharing records ownership + the realm scope against the (driver, connection), and a
        // user-owned connection (no row) reads back as `None`. Selectors + metadata only.
        let conn = migrated_conn();
        assert!(db_get_shared_connection(&conn, "github", "team").is_none());

        db_share_connection(&conn, "github", "team", "/projects/acme/**", "a@b.com").unwrap();
        let row = db_get_shared_connection(&conn, "github", "team").expect("shared");
        assert_eq!(row.scope, "/projects/acme/**");
        assert_eq!(row.shared_by, "a@b.com");
        assert!(!row.created_at.is_empty());

        // Independent per (driver, connection).
        assert!(db_get_shared_connection(&conn, "github", "personal").is_none());
        assert!(db_get_shared_connection(&conn, "slack", "team").is_none());

        // Last-writer-wins on re-share (e.g. a re-scope).
        db_share_connection(&conn, "github", "team", "/projects/beta/**", "c@d.com").unwrap();
        assert_eq!(
            db_get_shared_connection(&conn, "github", "team")
                .unwrap()
                .scope,
            "/projects/beta/**"
        );

        // The registry stores NO credential material — only the metadata columns exist.
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info('shared_connection')")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(
            !cols.iter().any(|c| c.contains("secret")
                || c.contains("token")
                || c.contains("ciphertext")
                || c.contains("nonce")),
            "the shared-connection registry must carry no secret column, got {cols:?}"
        );

        // Unsharing reverts to user-owned (idempotent).
        db_unshare_connection(&conn, "github", "team").unwrap();
        assert!(db_get_shared_connection(&conn, "github", "team").is_none());
        db_unshare_connection(&conn, "github", "team").unwrap();
    }

    #[test]
    fn list_reflects_owner_scope_from_the_shared_registry() {
        // t81: `list` LEFT JOINs the shared registry so each record carries its owner — a connection
        // with a shared row is project-owned; the rest stay user-owned. Metadata only (no decrypt).
        let store = SqliteSecrets::open_or_init(migrated_conn(), &Secret::from("pass")).unwrap();
        store
            .put(&ckey("github", "team"), Secret::from("ghp_team"))
            .unwrap();
        store
            .put(&ckey("github", "mine"), Secret::from("ghp_mine"))
            .unwrap();

        // Share only `github/team`.
        {
            let conn = store.lock().unwrap();
            db_share_connection(&conn, "github", "team", "/projects/acme/**", "a@b.com").unwrap();
        }

        let listed = store.list(Some(&DriverId::new("github"))).unwrap();
        let team = listed
            .iter()
            .find(|r| r.connection.as_str() == "team")
            .unwrap();
        let mine = listed
            .iter()
            .find(|r| r.connection.as_str() == "mine")
            .unwrap();
        assert_eq!(
            team.owner_scope,
            OwnerScope::Project,
            "shared ⇒ project-owned"
        );
        assert!(team.is_shared());
        assert_eq!(mine.owner_scope, OwnerScope::Me, "unshared ⇒ user-owned");
        assert!(!mine.is_shared());

        // The list view never carries the secret value (redaction holds across the join).
        let dump = format!("{listed:?}");
        assert!(!dump.contains("ghp_team") && !dump.contains("ghp_mine"));
    }

    #[test]
    fn list_shared_connections_returns_metadata_only() {
        let conn = migrated_conn();
        db_share_connection(&conn, "github", "team", "/projects/acme/**", "a@b.com").unwrap();
        db_share_connection(&conn, "slack", "ops", "/projects/acme/ops/**", "a@b.com").unwrap();
        let all = db_list_shared_connections(&conn);
        assert_eq!(all.len(), 2);
        // Ordered by (driver, connection): github before slack.
        assert_eq!(all[0].0, "github");
        assert_eq!(all[0].2.scope, "/projects/acme/**");
        assert_eq!(all[1].0, "slack");
    }

    #[test]
    fn broker_connection_records_team_binding_and_carries_no_secret() {
        // t66 / M9: provisioning a team connection records its brokering metadata against the
        // (driver, connection) — the team, provider, the broker's PUBLIC client id, the scope, who
        // provisioned it. Selectors + metadata only; no passphrase, no token.
        let conn = migrated_conn();
        assert!(db_get_broker_connection(&conn, "gdrive", "team").is_none());

        db_record_broker_connection(
            &conn,
            "gdrive",
            "team",
            "acme",
            "google",
            "qfs-cloud-broker-google",
            "drive.readonly",
            "alice@acme.co",
        )
        .unwrap();
        let row = db_get_broker_connection(&conn, "gdrive", "team").expect("brokered");
        assert_eq!(row.team, "acme");
        assert_eq!(row.provider, "google");
        assert_eq!(row.broker_client_id, "qfs-cloud-broker-google");
        assert_eq!(row.scope, "drive.readonly");
        assert_eq!(row.brokered_by, "alice@acme.co");
        assert!(!row.created_at.is_empty());

        // Independent per (driver, connection); last-writer-wins on re-provision (e.g. a re-team).
        assert!(db_get_broker_connection(&conn, "gdrive", "personal").is_none());
        db_record_broker_connection(
            &conn,
            "gdrive",
            "team",
            "beta",
            "google",
            "qfs-cloud-broker-google",
            "drive.readonly",
            "carol@beta.co",
        )
        .unwrap();
        assert_eq!(
            db_get_broker_connection(&conn, "gdrive", "team")
                .unwrap()
                .team,
            "beta"
        );

        // The registry stores NO credential material — only the metadata columns exist.
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info('broker_connection')")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(
            !cols.iter().any(|c| c.contains("secret")
                || c.contains("token")
                || c.contains("ciphertext")
                || c.contains("nonce")),
            "the broker registry must carry no secret column, got {cols:?}"
        );
    }

    // ==== ADR 0008 §5 — the KeyGuardian vault-key slots (EPIC 20260702120000/120020) ==========

    /// Enrolling a slot wraps the SAME DEK under one more KEK without re-sealing a single value:
    /// the sealed secret still opens, the slot count grows, and the store now opens through
    /// EITHER guardian alone (the point of the slot model).
    #[test]
    fn enroll_unlocks_via_either_slot_without_resealing() {
        use qfs_store::{migrate, Db, MemorySource, PROJECT_MIGRATIONS};
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        let conn = db.into_connection();

        let store = SqliteSecrets::open_or_init(conn, &Secret::from("pass-A")).unwrap();
        store
            .put(&ckey("github", "work"), Secret::from("ghp_tok"))
            .unwrap();

        // Enroll a raw-KEK guardian slot (what the keychain guardian does).
        let kek = qfs_secrets::generate_dek();
        let slot = store.enroll_slot(GUARDIAN_KEYCHAIN, &kek, None).unwrap();
        assert!(slot > 1, "the new slot sits beside the passphrase slot");
        assert_eq!(store.list_slots().unwrap().len(), 2);
        // The value sealed BEFORE the enroll still opens (no re-seal happened).
        assert_eq!(
            store.get(&ckey("github", "work")).unwrap().expose(),
            b"ghp_tok"
        );
        let conn = store.into_connection();

        // Open via the keychain KEK alone (no passphrase anywhere).
        let store = SqliteSecrets::open_with_resolver(conn, |s| {
            (s.guardian_kind == GUARDIAN_KEYCHAIN).then_some(kek)
        })
        .unwrap();
        assert_eq!(
            store.get(&ckey("github", "work")).unwrap().expose(),
            b"ghp_tok"
        );
        let conn = store.into_connection();

        // And via the passphrase alone (keychain unavailable — the headless host).
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("pass-A")).unwrap();
        assert_eq!(
            store.get(&ckey("github", "work")).unwrap().expose(),
            b"ghp_tok"
        );
    }

    /// A passphrase rekey is SLOT-SCOPED: the keychain slot keeps unlocking across it, the new
    /// passphrase works, and the old one is locked out.
    #[test]
    fn rekey_of_the_passphrase_slot_leaves_other_slots_working() {
        use qfs_store::{migrate, Db, MemorySource, PROJECT_MIGRATIONS};
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        let conn = db.into_connection();

        let store = SqliteSecrets::open_or_init(conn, &Secret::from("old-pass")).unwrap();
        store
            .put(&ckey("slack", "team"), Secret::from("xoxb"))
            .unwrap();
        let kek = qfs_secrets::generate_dek();
        store.enroll_slot(GUARDIAN_KEYCHAIN, &kek, None).unwrap();

        store
            .rewrap_passphrase(&Secret::from("old-pass"), &Secret::from("new-pass"))
            .unwrap();
        let conn = store.into_connection();

        // The keychain slot is untouched by the passphrase rekey.
        let store = SqliteSecrets::open_with_resolver(conn, |s| {
            (s.guardian_kind == GUARDIAN_KEYCHAIN).then_some(kek)
        })
        .unwrap();
        assert_eq!(store.get(&ckey("slack", "team")).unwrap().expose(), b"xoxb");
        let conn = store.into_connection();

        // The new passphrase opens; the old one is locked out.
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("new-pass")).unwrap();
        let conn = store.into_connection();
        match SqliteSecrets::open_or_init(conn, &Secret::from("old-pass")) {
            Err(SecretError::Locked) => {}
            other => panic!(
                "old passphrase must be locked out, got {other:?}",
                other = other.map(|_| ())
            ),
        }
    }

    /// A wrong passphrase on a multi-slot store is the single, slot-anonymous `Locked` — and the
    /// last remaining slot refuses revocation (a store with no slot could never open again).
    #[test]
    fn wrong_passphrase_is_locked_and_the_last_slot_is_irrevocable() {
        use qfs_store::{migrate, Db, MemorySource, PROJECT_MIGRATIONS};
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        let conn = db.into_connection();

        let store = SqliteSecrets::open_or_init(conn, &Secret::from("right")).unwrap();
        let kek = qfs_secrets::generate_dek();
        let keychain_slot = store.enroll_slot(GUARDIAN_KEYCHAIN, &kek, None).unwrap();
        let conn = store.into_connection();

        // Wrong passphrase + keychain unavailable: Locked, with no hint of WHICH slot failed.
        match SqliteSecrets::open_or_init(conn, &Secret::from("wrong")) {
            Err(SecretError::Locked) => {}
            other => panic!("expected Locked, got {other:?}", other = other.map(|_| ())),
        }

        // Reopen rightfully; revoke down to one slot; the last slot is refused.
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        let store = SqliteSecrets::open_or_init(db.into_connection(), &Secret::from("p")).unwrap();
        let extra = store.enroll_slot(GUARDIAN_KEYCHAIN, &kek, None).unwrap();
        store.revoke_slot(extra).unwrap();
        let slots = store.list_slots().unwrap();
        assert_eq!(slots.len(), 1);
        let last = slots[0].0;
        assert!(
            store.revoke_slot(last).is_err(),
            "the last slot must be irrevocable"
        );
        let _ = keychain_slot;
    }

    /// The v10 forward-copy: a PRE-v10 store (its wrap in the single `secret_meta` row, its value
    /// sealed in `secret_store`) opens after the full migration set with its EXISTING passphrase,
    /// and the value still decrypts. `secret_meta` ends empty (the slot table is the one source of
    /// truth from v10 on).
    #[test]
    fn pre_v10_store_opens_after_the_forward_copy_migration() {
        use qfs_store::{migrate, Db, MemorySource, PROJECT_MIGRATIONS};
        let mut db = Db::open(&MemorySource).unwrap();
        // A v9 world: everything up to (and including) the mount-coordinate migration.
        migrate(&mut db, &PROJECT_MIGRATIONS[..9]).unwrap();

        // Build the pre-v10 store shape by hand with the same envelope primitives the old
        // `open_or_init` used: a DEK wrapped under the passphrase KEK in `secret_meta`, and a
        // value sealed under the DEK in `secret_store`.
        let dek = qfs_secrets::generate_dek();
        let salt = generate_salt();
        let kek = derive_kek(b"legacy-pass", &salt).unwrap();
        let wrapped = wrap_dek(&kek, &dek).unwrap();
        db.conn()
            .execute(
                "INSERT INTO secret_meta (id, wrapped_dek, kdf_salt) VALUES (1, ?1, ?2)",
                rusqlite::params![wrapped, salt.as_slice()],
            )
            .unwrap();
        let (nonce, ct) = seal(&dek, b"legacy-token").unwrap();
        db.conn()
            .execute(
                "INSERT INTO secret_store (driver, connection, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["github", "work", nonce.as_slice(), ct],
            )
            .unwrap();

        // The upgrade: the full migration set (v10 copies the wrap into slot #1 + empties
        // secret_meta).
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        let conn = db.into_connection();
        let meta_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM secret_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(meta_rows, 0, "secret_meta is emptied by the forward-copy");

        // The existing passphrase still opens the store, and the sealed value still decrypts.
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("legacy-pass")).unwrap();
        assert_eq!(
            store.get(&ckey("github", "work")).unwrap().expose(),
            b"legacy-token"
        );
    }
}
