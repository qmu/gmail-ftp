//! t46: the **rusqlite [`SessionStore`] impl** over the System DB.
//!
//! This lives in `qfs-store` (not in `qfs-session`) on purpose: `qfs-session` is a pure-ish leaf
//! with NO rusqlite/tokio, and `qfs-store` is the crate that owns a real `rusqlite::Connection`. So
//! the consumer-side trait is defined up in the domain leaf and its SQLite implementation is injected
//! here — the same split t45 uses for the identity store (`SqliteIdentityStore`). The
//! `qfs-store -> qfs-session` edge is acyclic (qfs-session depends on neither qfs-store nor rusqlite).
//!
//! ## Token hygiene + expiry (RFD §10)
//! The store trades only in token HASHES — the plaintext token never reaches this crate. `expires_at`
//! is computed in SQL (`strftime(... '+N seconds')`) so the clock is the DB's; every [`lookup`]
//! filters on `expires_at > now` AND lazily reaps already-expired rows, so an expired session is
//! reported as absent. The fetched hash is re-verified against the lookup key with a CONSTANT-TIME
//! compare ([`qfs_session::SessionToken::matches_hash`]) as defense-in-depth on top of the indexed
//! `token_hash` fetch.
//!
//! [`lookup`]: SqliteSessionStore::lookup

use std::sync::Mutex;

use qfs_session::{Session, SessionError, SessionId, SessionStore, UserId};
use rusqlite::{Connection, OptionalExtension};

use crate::Db;

/// The System-DB-backed session store. Owns the migrated connection inside a `Mutex` so the whole
/// backend is `Send + Sync` (mirrors `SqliteIdentityStore`). Holds no token material.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Build the store over a migrated System-DB connection. The session migration (v4) must already
    /// be applied — `SystemDb::open` does that on start.
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Build the store over a migrated [`Db`] handle (consumes it for the owned connection). The
    /// ergonomic constructor for callers that already hold a `Db`.
    #[must_use]
    pub fn from_db(db: Db) -> Self {
        Self::new(db.into_connection())
    }

    /// Lock the connection mutex, mapping a poisoned lock to a secret-free backend error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SessionError> {
        self.conn
            .lock()
            .map_err(|_| SessionError::Backend("session store lock poisoned".into()))
    }
}

/// The SQL fragment for "now" in the fixed-width RFC-3339-UTC form the schema stamps. Inlined into
/// each statement so expiry comparisons use the DB's clock, not the caller's.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%SZ','now')";

/// Map a row `(token_hash, user_id, created_at, expires_at, rotated_from)` into an owned [`Session`].
fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: SessionId(r.get::<_, String>(0)?),
        user_id: UserId(r.get(1)?),
        created_at: r.get(2)?,
        expires_at: r.get(3)?,
        rotated_from: r.get::<_, Option<String>>(4)?.map(SessionId),
    })
}

impl SessionStore for SqliteSessionStore {
    fn create(
        &self,
        user_id: UserId,
        token_hash: &str,
        ttl_secs: i64,
    ) -> Result<Session, SessionError> {
        let conn = self.lock()?;
        // `expires_at` = now + ttl, computed in SQL (the DB clock). The ttl is bound as the SQLite
        // modifier string `'+N seconds'`; a non-positive ttl yields an already-expired session.
        let modifier = format!("+{ttl_secs} seconds");
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?3))",
            rusqlite::params![token_hash, user_id.0, modifier],
        )
        .map_err(|e| SessionError::Backend(format!("creating session: {e}")))?;
        conn.query_row(
            "SELECT token_hash, user_id, created_at, expires_at, rotated_from \
             FROM sessions WHERE token_hash = ?1",
            rusqlite::params![token_hash],
            row_to_session,
        )
        .map_err(|e| SessionError::Backend(format!("reading back created session: {e}")))
    }

    fn lookup(&self, token_hash: &str) -> Result<Option<Session>, SessionError> {
        let conn = self.lock()?;
        // Lazily reap every expired row (cheap, indexed on expires_at) so the table self-prunes and
        // an expired session can never resolve.
        conn.execute(
            &format!("DELETE FROM sessions WHERE expires_at <= {NOW_SQL}"),
            [],
        )
        .map_err(|e| SessionError::Backend(format!("reaping expired sessions: {e}")))?;
        // Fetch the LIVE row by its indexed hash key.
        let session = conn
            .query_row(
                &format!(
                    "SELECT token_hash, user_id, created_at, expires_at, rotated_from \
                     FROM sessions WHERE token_hash = ?1 AND expires_at > {NOW_SQL}"
                ),
                rusqlite::params![token_hash],
                row_to_session,
            )
            .optional()
            .map_err(|e| SessionError::Backend(format!("looking up session: {e}")))?;
        // Defense-in-depth: re-verify the fetched hash equals the lookup key in CONSTANT TIME (RFD
        // §10) on top of the indexed equality fetch. Both sides are token HASHES (not the live
        // token), compared via the workspace's single constant-time primitive.
        Ok(session.filter(|s| {
            qfs_crypto_core::constant_time_eq(s.id.as_str().as_bytes(), token_hash.as_bytes())
        }))
    }

    fn rotate(
        &self,
        old_token_hash: &str,
        new_token_hash: &str,
        ttl_secs: i64,
    ) -> Result<Session, SessionError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| SessionError::Backend(format!("starting rotate transaction: {e}")))?;

        // 1. The OLD session must still be live (rotate FROM an existing authentication state).
        let old_user: Option<i64> = tx
            .query_row(
                &format!(
                    "SELECT user_id FROM sessions WHERE token_hash = ?1 AND expires_at > {NOW_SQL}"
                ),
                rusqlite::params![old_token_hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SessionError::Backend(format!("reading the session to rotate: {e}")))?;
        let Some(user_id) = old_user else {
            return Err(SessionError::NotFound);
        };

        // 2. Mint the NEW session for the same user, linking `rotated_from` to the old hash.
        let modifier = format!("+{ttl_secs} seconds");
        tx.execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at, rotated_from) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?3), ?4)",
            rusqlite::params![new_token_hash, user_id, modifier, old_token_hash],
        )
        .map_err(|e| SessionError::Backend(format!("minting the rotated session: {e}")))?;

        // 3. Expire (delete) the OLD row so its token can no longer resolve.
        tx.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            rusqlite::params![old_token_hash],
        )
        .map_err(|e| SessionError::Backend(format!("expiring the old session: {e}")))?;

        let new_session = tx
            .query_row(
                "SELECT token_hash, user_id, created_at, expires_at, rotated_from \
                 FROM sessions WHERE token_hash = ?1",
                rusqlite::params![new_token_hash],
                row_to_session,
            )
            .map_err(|e| SessionError::Backend(format!("reading back rotated session: {e}")))?;
        tx.commit()
            .map_err(|e| SessionError::Backend(format!("committing rotate: {e}")))?;
        Ok(new_session)
    }

    fn revoke(&self, token_hash: &str) -> Result<bool, SessionError> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                rusqlite::params![token_hash],
            )
            .map_err(|e| SessionError::Backend(format!("revoking session: {e}")))?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{MemorySource, SystemDb};
    use qfs_session::SessionToken;

    /// Build a session store over a fresh in-memory System DB holding ONE user (a session FKs to
    /// `users`). An in-memory SQLite DB cannot be shared across two `Connection`s, so we open one
    /// System DB, seed the user on its owned connection, and hand that SAME connection to the store.
    fn store_with_user() -> (SqliteSessionStore, UserId) {
        let sys = SystemDb::open(&MemorySource).unwrap();
        let conn = sys.into_db().into_connection();
        conn.execute("INSERT INTO users (primary_email) VALUES ('a@b.com')", [])
            .unwrap();
        let uid = UserId(conn.last_insert_rowid());
        (SqliteSessionStore::new(conn), uid)
    }

    fn token(seed: &[u8]) -> SessionToken {
        SessionToken::from_entropy(seed)
    }

    #[test]
    fn create_then_lookup_resolves_the_session() {
        let (store, uid) = store_with_user();
        let t = token(&[1, 2, 3, 4]);
        let created = store.create(uid, &t.hash(), 3600).unwrap();
        assert_eq!(created.user_id, uid);
        assert_eq!(created.id.as_str(), t.hash());
        assert!(created.rotated_from.is_none());

        // Looking up by the token's hash (what the cookie carries, hashed) resolves the session.
        let found = store.lookup(&t.hash()).unwrap().expect("live session");
        assert_eq!(found.user_id, uid);
        // An unknown token hash resolves to nothing (not an error).
        assert!(store.lookup(&token(&[9, 9]).hash()).unwrap().is_none());
    }

    #[test]
    fn an_expired_session_is_treated_as_absent_and_reaped() {
        let (store, uid) = store_with_user();
        let t = token(&[5, 5, 5]);
        // ttl 0 → expires_at == now → already expired (expires_at > now is false).
        store.create(uid, &t.hash(), 0).unwrap();
        assert!(
            store.lookup(&t.hash()).unwrap().is_none(),
            "an expired session must not resolve"
        );
        // And it was lazily reaped: no row remains.
        let conn = store.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the expired row was reaped on lookup");
    }

    #[test]
    fn rotate_mints_a_new_token_links_the_old_and_invalidates_it() {
        let (store, uid) = store_with_user();
        let old = token(&[1, 1, 1, 1]);
        let new = token(&[2, 2, 2, 2]);
        store.create(uid, &old.hash(), 3600).unwrap();

        let rotated = store.rotate(&old.hash(), &new.hash(), 3600).unwrap();
        assert_eq!(rotated.user_id, uid);
        assert_eq!(rotated.id.as_str(), new.hash());
        // The audit breadcrumb points at the old session's hash.
        assert_eq!(
            rotated.rotated_from.as_ref().map(SessionId::as_str),
            Some(old.hash().as_str())
        );
        // The OLD token no longer resolves; the NEW one does.
        assert!(store.lookup(&old.hash()).unwrap().is_none());
        assert!(store.lookup(&new.hash()).unwrap().is_some());
    }

    #[test]
    fn rotate_on_a_missing_session_is_not_found() {
        let (store, _uid) = store_with_user();
        let err = store
            .rotate(&token(&[0]).hash(), &token(&[1]).hash(), 3600)
            .unwrap_err();
        assert_eq!(err, SessionError::NotFound);
    }

    #[test]
    fn revoke_deletes_the_session_and_is_idempotent() {
        let (store, uid) = store_with_user();
        let t = token(&[7, 7]);
        store.create(uid, &t.hash(), 3600).unwrap();
        assert!(store.revoke(&t.hash()).unwrap(), "first revoke removes it");
        assert!(store.lookup(&t.hash()).unwrap().is_none());
        assert!(
            !store.revoke(&t.hash()).unwrap(),
            "second revoke removes nothing (idempotent)"
        );
    }

    #[test]
    fn a_session_must_belong_to_a_real_user() {
        let (store, _uid) = store_with_user();
        // user_id 999 has no `users` row → the FK rejects it as a structured backend error.
        let err = store
            .create(UserId(999), &token(&[3]).hash(), 3600)
            .unwrap_err();
        assert!(
            matches!(err, SessionError::Backend(_)),
            "an unknown user is a backend (FK) error: {err:?}"
        );
    }
}
