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
    derive_kek, generate_dek, generate_salt, open, seal, unwrap_dek, wrap_dek, ConnectionId,
    ConnectionRecord, CredentialKey, DriverId, OwnerScope, Secret, SecretError, Secrets,
};
use rusqlite::{Connection, OptionalExtension};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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
        let meta: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT wrapped_dek, kdf_salt FROM secret_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| SecretError::Backend(format!("reading secret metadata: {e}")))?;

        let dek = match meta {
            // Established store: re-derive the KEK and unwrap the SAME DEK. A wrong passphrase /
            // tampered meta fails authentication -> Locked (no bytes leaked).
            Some((wrapped, salt)) => {
                let kek =
                    derive_kek(passphrase.expose(), &salt).map_err(|_| SecretError::Locked)?;
                unwrap_dek(&kek, &wrapped).map_err(|_| SecretError::Locked)?
            }
            // Fresh store: mint a DEK + salt, wrap the DEK under the passphrase KEK, persist once.
            None => {
                let dek = generate_dek();
                let salt = generate_salt();
                let kek =
                    derive_kek(passphrase.expose(), &salt).map_err(|_| SecretError::Locked)?;
                let wrapped = wrap_dek(&kek, &dek)
                    .map_err(|_| SecretError::Backend("wrapping the data key".into()))?;
                conn.execute(
                    "INSERT INTO secret_meta (id, wrapped_dek, kdf_salt) VALUES (1, ?1, ?2)",
                    rusqlite::params![wrapped, salt.as_slice()],
                )
                .map_err(|e| SecretError::Backend(format!("initializing secret metadata: {e}")))?;
                dek
            }
        };

        Ok(Self {
            conn: Mutex::new(conn),
            dek,
        })
    }

    /// Lock the connection mutex, mapping a poisoned lock to a secret-free backend error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SecretError> {
        self.conn
            .lock()
            .map_err(|_| SecretError::Backend("secret store lock poisoned".into()))
    }
}

impl Secrets for SqliteSecrets {
    fn get(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        let conn = self.lock()?;
        let row: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT nonce, ciphertext FROM secret_store WHERE driver = ?1 AND connection = ?2",
                rusqlite::params![key.driver.as_str(), key.connection.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| SecretError::Backend(format!("reading credential: {e}")))?;
        match row {
            Some((nonce, ciphertext)) => {
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

/// Set (UPSERT) the active connection for `driver` in the `active_account` table — last-writer-wins,
/// the same semantics as the old `credentials.active` sidecar. Selectors only; no passphrase.
///
/// # Errors
/// [`SecretError::Backend`] on a DB failure (secret-free message).
pub fn db_set_active(conn: &Connection, driver: &str, connection: &str) -> Result<(), SecretError> {
    conn.execute(
        "INSERT INTO active_account (driver, connection) VALUES (?1, ?2) \
         ON CONFLICT(driver) DO UPDATE SET connection = excluded.connection",
        rusqlite::params![driver, connection],
    )
    .map_err(|e| SecretError::Backend(format!("setting active connection: {e}")))?;
    Ok(())
}

/// Read the active connection for `driver` from the `active_account` table, or `None` if unset /
/// unreadable. Best-effort (selectors only; no passphrase) so the commit resolver can fall back.
#[must_use]
pub fn db_get_active(conn: &Connection, driver: &str) -> Option<String> {
    conn.query_row(
        "SELECT connection FROM active_account WHERE driver = ?1",
        rusqlite::params![driver],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
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
/// no passphrase (the same passphrase-free path as `active_account`).
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
/// material, so it needs no passphrase (the same passphrase-free path as `active_account`).
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
/// metadata only, never a secret. Powers a `connection list --project` / `/sys` style surface that
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

    #[test]
    fn active_connection_set_get_round_trip() {
        let conn = migrated_conn();
        assert!(db_get_active(&conn, "mail").is_none());
        db_set_active(&conn, "mail", "work").unwrap();
        assert_eq!(db_get_active(&conn, "mail").as_deref(), Some("work"));
        // Last-writer-wins (UPSERT keeps one row per driver).
        db_set_active(&conn, "mail", "personal").unwrap();
        assert_eq!(db_get_active(&conn, "mail").as_deref(), Some("personal"));
        // Other drivers are independent.
        db_set_active(&conn, "s3", "prod").unwrap();
        assert_eq!(db_get_active(&conn, "s3").as_deref(), Some("prod"));
        assert_eq!(db_get_active(&conn, "mail").as_deref(), Some("personal"));
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
}
