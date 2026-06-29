//! t49: the **rusqlite OAuth-flow store** over the System DB — registered clients (RFC 7591), the
//! short-lived single-use authorization codes (auth-code + PKCE grant), and the refresh-token handle
//! skeleton.
//!
//! Like [`crate::oauth_key_store`] / [`crate::identity_store`] / [`crate::session_store`], this lives
//! in `qfs-store` (the crate that owns a real `rusqlite::Connection`), NOT in the pure `qfs-oauth`
//! domain leaf: the protocol decisions (PKCE verification, code minting, the token response) stay in
//! `qfs-oauth` and their SQLite persistence is here. This store trades only in OPAQUE strings + code
//! HASHES — the binary bridges `qfs-oauth` ↔ this store, so `qfs-store` gains no `qfs-oauth` edge.
//!
//! ## At-rest hygiene (RFD §10)
//! Authorization CODES, refresh-token HANDLES, and client SECRETS are stored ONLY as their
//! `sha256_hex` (preimage-resistant) — the plaintext exists only in the redirect/response that
//! delivers it once. A System-DB leak therefore yields no usable codes/handles/secrets. Codes are
//! short-lived (the caller passes a ~60s ttl), single-use (burned on first exchange via a delete in
//! the SAME transaction that reads them — a replay finds nothing), and the lookup also lazily reaps
//! every expired row so an expired code can never resolve.

use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::{Db, StoreError};

/// A dynamically-registered client as it leaves the store. `redirect_uris` is the exact allowlist a
/// `redirect_uri` is matched against (open-redirect defense) — split back into the registered URIs.
pub struct RegisteredClient {
    /// The minted public client identifier.
    pub client_id: String,
    /// The exact registered redirect-URI allowlist (no wildcard/substring matching downstream).
    pub redirect_uris: Vec<String>,
    /// The optional human label from the registration request.
    pub client_name: Option<String>,
}

/// A redeemed authorization code's bound context, returned by [`SqliteOauthFlowStore::take_code`].
/// Carries everything the token endpoint re-checks: the exact `client_id` + `redirect_uri`, the PKCE
/// `challenge`/`method` the presented verifier is checked against, the authenticated `user_id` the
/// token is minted for, and the granted `scope`.
pub struct RedeemedCode {
    pub client_id: String,
    pub user_id: i64,
    pub redirect_uri: String,
    pub pkce_challenge: String,
    pub pkce_method: String,
    pub scope: String,
}

/// A redeemed refresh-token handle's bound context, returned by [`SqliteOauthFlowStore::take_refresh`].
/// Carries everything the refresh grant re-mints an access token for: the `user_id` the token is for,
/// the exact `client_id` it was issued to (re-checked at the token endpoint), and the granted `scope`.
pub struct RedeemedRefresh {
    pub user_id: i64,
    pub client_id: String,
    pub scope: String,
}

/// The redirect-URI list field separator (newline). A redirect URI never contains a newline, so a
/// newline-joined column round-trips the exact allowlist without pulling a JSON dependency into this
/// sync leaf.
const URI_SEP: char = '\n';

/// The System-DB-backed OAuth-flow store. Owns the migrated connection inside a `Mutex` (so the whole
/// backend is `Send + Sync`, mirroring the sibling stores). Holds no key material — only opaque ids +
/// code/handle HASHES.
pub struct SqliteOauthFlowStore {
    conn: Mutex<Connection>,
}

/// The SQL fragment for "now" in the fixed-width RFC-3339-UTC form the schema stamps — so expiry
/// comparisons use the DB's clock, not the caller's.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%SZ','now')";

impl SqliteOauthFlowStore {
    /// Build the store over a migrated System-DB connection. The OAuth-flow migration (v6) must
    /// already be applied — `SystemDb::open` does that on start.
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Build the store over a migrated [`Db`] handle (consumes it for the owned connection).
    #[must_use]
    pub fn from_db(db: Db) -> Self {
        Self::new(db.into_connection())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Sqlite("oauth flow store lock poisoned".to_string()))
    }

    /// Persist a freshly registered client (RFC 7591). `redirect_uris` is the validated exact
    /// allowlist; `client_secret_hash` is `None` for a public PKCE client (the MCP norm) or the
    /// `sha256_hex` of a secret — never the raw secret.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure (e.g. a duplicate `client_id`).
    pub fn register_client(
        &self,
        client_id: &str,
        redirect_uris: &[String],
        client_name: Option<&str>,
        client_secret_hash: Option<&str>,
    ) -> Result<(), StoreError> {
        let joined = redirect_uris.join(&URI_SEP.to_string());
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oauth_clients (client_id, redirect_uris, client_name, client_secret_hash) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![client_id, joined, client_name, client_secret_hash],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// Look a registered client up by `client_id`, returning its exact redirect-URI allowlist.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn find_client(&self, client_id: &str) -> Result<Option<RegisteredClient>, StoreError> {
        let conn = self.lock()?;
        let row: Option<(String, String, Option<String>)> = conn
            .query_row(
                "SELECT client_id, redirect_uris, client_name FROM oauth_clients WHERE client_id = ?1",
                rusqlite::params![client_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        Ok(
            row.map(|(client_id, joined, client_name)| RegisteredClient {
                client_id,
                redirect_uris: joined.split(URI_SEP).map(str::to_string).collect(),
                client_name,
            }),
        )
    }

    /// Insert a short-lived authorization code, keyed by its `code_hash` (`sha256_hex(code)` — never
    /// the plaintext), bound to the exact client + redirect + PKCE challenge + authenticated user +
    /// scope. `ttl_secs` is the short TTL (the caller passes ~60s); `expires_at` is computed in SQL.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure (e.g. an unknown client/user FK).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_code(
        &self,
        code_hash: &str,
        client_id: &str,
        user_id: i64,
        redirect_uri: &str,
        pkce_challenge: &str,
        pkce_method: &str,
        scope: &str,
        ttl_secs: i64,
    ) -> Result<(), StoreError> {
        let modifier = format!("+{ttl_secs} seconds");
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oauth_codes \
             (code_hash, client_id, user_id, redirect_uri, pkce_challenge, pkce_method, scope, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?8))",
            rusqlite::params![
                code_hash,
                client_id,
                user_id,
                redirect_uri,
                pkce_challenge,
                pkce_method,
                scope,
                modifier
            ],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// Atomically REDEEM an authorization code: read its bound context and DELETE it in ONE
    /// transaction (single-use — a replay finds nothing). Returns `None` if the code is unknown,
    /// already redeemed, or expired (expired rows are also reaped). The row is deleted on EVERY match
    /// regardless of expiry, so a presented-but-expired code is still burned.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn take_code(&self, code_hash: &str) -> Result<Option<RedeemedCode>, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(StoreError::from)?;
        // Lazily reap every OTHER expired code so the table self-prunes.
        tx.execute(
            &format!("DELETE FROM oauth_codes WHERE expires_at <= {NOW_SQL} AND code_hash <> ?1"),
            rusqlite::params![code_hash],
        )
        .map_err(StoreError::from)?;
        // Read this code's bound context (whether or not it is expired — we burn it either way).
        let row: Option<(String, i64, String, String, String, String, String)> = tx
            .query_row(
                "SELECT client_id, user_id, redirect_uri, pkce_challenge, pkce_method, scope, expires_at \
                 FROM oauth_codes WHERE code_hash = ?1",
                rusqlite::params![code_hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)?;
        // Burn the row (single-use) and decide whether it was still live.
        let redeemed = match row {
            None => None,
            Some((
                client_id,
                user_id,
                redirect_uri,
                pkce_challenge,
                pkce_method,
                scope,
                expires_at,
            )) => {
                tx.execute(
                    "DELETE FROM oauth_codes WHERE code_hash = ?1",
                    rusqlite::params![code_hash],
                )
                .map_err(StoreError::from)?;
                // Compare expiry against the DB clock read in the SAME transaction.
                let now: String = tx
                    .query_row(&format!("SELECT {NOW_SQL}"), [], |r| r.get(0))
                    .map_err(StoreError::from)?;
                if expires_at.as_str() > now.as_str() {
                    Some(RedeemedCode {
                        client_id,
                        user_id,
                        redirect_uri,
                        pkce_challenge,
                        pkce_method,
                        scope,
                    })
                } else {
                    None
                }
            }
        };
        tx.commit().map_err(StoreError::from)?;
        Ok(redeemed)
    }

    /// Insert a refresh-token handle (issued at the auth-code token exchange; ROTATED on refresh in
    /// t50). Keyed by `handle_hash` (`sha256_hex(handle)` — never the plaintext). `ttl_secs` is the
    /// refresh lifetime. `rotated_from` is `NULL` for a first-issue handle (the initial code exchange).
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn insert_refresh(
        &self,
        handle_hash: &str,
        user_id: i64,
        client_id: &str,
        scope: &str,
        ttl_secs: i64,
    ) -> Result<(), StoreError> {
        let modifier = format!("+{ttl_secs} seconds");
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oauth_refresh_tokens (handle_hash, user_id, client_id, scope, expires_at) \
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?5))",
            rusqlite::params![handle_hash, user_id, client_id, scope, modifier],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// Insert a ROTATED refresh-token handle: the successor minted when a refresh grant burns the
    /// presented handle. Identical to [`insert_refresh`](Self::insert_refresh) but records
    /// `rotated_from` (the prior handle's hash) so the rotation lineage is auditable. Never carries the
    /// plaintext of either handle.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn insert_refresh_rotated(
        &self,
        handle_hash: &str,
        user_id: i64,
        client_id: &str,
        scope: &str,
        ttl_secs: i64,
        rotated_from: &str,
    ) -> Result<(), StoreError> {
        let modifier = format!("+{ttl_secs} seconds");
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO oauth_refresh_tokens \
             (handle_hash, user_id, client_id, scope, expires_at, rotated_from) \
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?5), ?6)",
            rusqlite::params![
                handle_hash,
                user_id,
                client_id,
                scope,
                modifier,
                rotated_from
            ],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    /// Atomically REDEEM a refresh-token handle for rotation: read its bound context and DELETE it in
    /// ONE transaction (single-use — the presented handle is burned so a replay finds nothing). Returns
    /// `None` if the handle is unknown, already rotated/redeemed, or expired (expired rows are also
    /// reaped). The row is deleted on EVERY match regardless of expiry, so a presented-but-expired
    /// handle is still burned.
    ///
    /// ## Single-use rotation (the security property)
    /// The caller mints a NEW handle (via [`insert_refresh_rotated`](Self::insert_refresh_rotated))
    /// on a successful redeem and returns it to the client; the OLD handle no longer exists, so
    /// presenting it again (a leaked/stale handle replay) resolves to `None` → `invalid_grant`. Full
    /// reuse-detection that REVOKES the whole token family on a replay (rather than just rejecting the
    /// one handle) would require retaining burned rows behind a `revoked_at` column — a documented
    /// follow-up (it needs a new migration); the single-use burn already rejects every replay.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on a DB failure.
    pub fn take_refresh(&self, handle_hash: &str) -> Result<Option<RedeemedRefresh>, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(StoreError::from)?;
        // Lazily reap every OTHER expired handle so the table self-prunes.
        tx.execute(
            &format!(
                "DELETE FROM oauth_refresh_tokens WHERE expires_at <= {NOW_SQL} AND handle_hash <> ?1"
            ),
            rusqlite::params![handle_hash],
        )
        .map_err(StoreError::from)?;
        // Read this handle's bound context (whether or not it is expired — we burn it either way).
        let row: Option<(i64, String, String, String)> = tx
            .query_row(
                "SELECT user_id, client_id, scope, expires_at \
                 FROM oauth_refresh_tokens WHERE handle_hash = ?1",
                rusqlite::params![handle_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        // Burn the row (single-use) and decide whether it was still live.
        let redeemed = match row {
            None => None,
            Some((user_id, client_id, scope, expires_at)) => {
                tx.execute(
                    "DELETE FROM oauth_refresh_tokens WHERE handle_hash = ?1",
                    rusqlite::params![handle_hash],
                )
                .map_err(StoreError::from)?;
                let now: String = tx
                    .query_row(&format!("SELECT {NOW_SQL}"), [], |r| r.get(0))
                    .map_err(StoreError::from)?;
                if expires_at.as_str() > now.as_str() {
                    Some(RedeemedRefresh {
                        user_id,
                        client_id,
                        scope,
                    })
                } else {
                    None
                }
            }
        };
        tx.commit().map_err(StoreError::from)?;
        Ok(redeemed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{MemorySource, SystemDb};

    /// A flow store over a fresh in-memory System DB holding ONE user (codes FK to `users`). An
    /// in-memory SQLite DB cannot be shared across two `Connection`s, so we open one System DB, seed
    /// the user on its owned connection, and hand that SAME connection to the store.
    fn store_with_user() -> (SqliteOauthFlowStore, i64) {
        let sys = SystemDb::open(&MemorySource).unwrap();
        let conn = sys.into_db().into_connection();
        conn.execute("INSERT INTO users (primary_email) VALUES ('a@b.com')", [])
            .unwrap();
        let uid = conn.last_insert_rowid();
        (SqliteOauthFlowStore::new(conn), uid)
    }

    fn register(store: &SqliteOauthFlowStore) {
        store
            .register_client(
                "client-1",
                &["https://app.example/cb".to_string()],
                Some("Test App"),
                None,
            )
            .unwrap();
    }

    #[test]
    fn register_then_find_round_trips_the_redirect_allowlist() {
        let (store, _uid) = store_with_user();
        store
            .register_client(
                "c-multi",
                &[
                    "https://a.example/cb".to_string(),
                    "http://localhost:1234/cb".to_string(),
                ],
                None,
                None,
            )
            .unwrap();
        let found = store.find_client("c-multi").unwrap().expect("registered");
        assert_eq!(found.client_id, "c-multi");
        assert_eq!(
            found.redirect_uris,
            vec!["https://a.example/cb", "http://localhost:1234/cb"]
        );
        assert!(store.find_client("nope").unwrap().is_none());
    }

    #[test]
    fn a_code_is_single_use_and_replay_finds_nothing() {
        let (store, uid) = store_with_user();
        register(&store);
        store
            .insert_code(
                "codehash-A",
                "client-1",
                uid,
                "https://app.example/cb",
                "challenge-xyz",
                "S256",
                "mcp:read",
                60,
            )
            .unwrap();
        let first = store
            .take_code("codehash-A")
            .unwrap()
            .expect("first redeem");
        assert_eq!(first.client_id, "client-1");
        assert_eq!(first.user_id, uid);
        assert_eq!(first.redirect_uri, "https://app.example/cb");
        assert_eq!(first.pkce_challenge, "challenge-xyz");
        assert_eq!(first.pkce_method, "S256");
        assert_eq!(first.scope, "mcp:read");
        // Replay: the code was burned on first redemption.
        assert!(
            store.take_code("codehash-A").unwrap().is_none(),
            "replay rejected"
        );
    }

    #[test]
    fn an_expired_code_does_not_redeem_but_is_still_burned() {
        let (store, uid) = store_with_user();
        register(&store);
        // ttl 0 → expires_at == now → already expired.
        store
            .insert_code(
                "codehash-exp",
                "client-1",
                uid,
                "https://app.example/cb",
                "ch",
                "S256",
                "",
                0,
            )
            .unwrap();
        assert!(
            store.take_code("codehash-exp").unwrap().is_none(),
            "an expired code must not redeem"
        );
        // It was burned regardless, so no row lingers.
        let conn = store.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM oauth_codes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the expired code row was burned");
    }

    #[test]
    fn insert_refresh_handle_persists_only_a_hash() {
        let (store, uid) = store_with_user();
        register(&store);
        store
            .insert_refresh("refresh-hash-1", uid, "client-1", "mcp:read", 3600)
            .unwrap();
        let conn = store.lock().unwrap();
        let stored: String = conn
            .query_row("SELECT handle_hash FROM oauth_refresh_tokens", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, "refresh-hash-1");
    }

    #[test]
    fn the_unknown_code_is_a_plain_none_not_an_error() {
        let (store, _uid) = store_with_user();
        assert!(store.take_code("never-issued").unwrap().is_none());
    }

    #[test]
    fn a_refresh_handle_is_single_use_and_replay_finds_nothing() {
        let (store, uid) = store_with_user();
        register(&store);
        store
            .insert_refresh("rh-A", uid, "client-1", "mcp:read", 3600)
            .unwrap();
        let first = store.take_refresh("rh-A").unwrap().expect("first redeem");
        assert_eq!(first.user_id, uid);
        assert_eq!(first.client_id, "client-1");
        assert_eq!(first.scope, "mcp:read");
        // Replay: the handle was burned on first redemption (single-use rotation).
        assert!(
            store.take_refresh("rh-A").unwrap().is_none(),
            "a rotated/replayed refresh handle must not redeem"
        );
    }

    #[test]
    fn an_expired_refresh_handle_does_not_redeem_but_is_still_burned() {
        let (store, uid) = store_with_user();
        register(&store);
        store
            .insert_refresh("rh-exp", uid, "client-1", "", 0)
            .unwrap();
        assert!(
            store.take_refresh("rh-exp").unwrap().is_none(),
            "an expired refresh handle must not redeem"
        );
        let conn = store.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM oauth_refresh_tokens", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "the expired refresh handle row was burned");
    }

    #[test]
    fn a_rotated_handle_records_its_lineage() {
        let (store, uid) = store_with_user();
        register(&store);
        store
            .insert_refresh_rotated("rh-new", uid, "client-1", "mcp:read", 3600, "rh-old")
            .unwrap();
        let conn = store.lock().unwrap();
        let from: String = conn
            .query_row(
                "SELECT rotated_from FROM oauth_refresh_tokens WHERE handle_hash = 'rh-new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(from, "rh-old");
    }
}
