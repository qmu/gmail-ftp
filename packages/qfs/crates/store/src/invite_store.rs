//! t55: the **rusqlite [`InviteStore`] impl** over the System DB.
//!
//! This lives in `qfs-store` (not in `qfs-identity`) on purpose: `qfs-identity` is a pure-ish leaf
//! with NO rusqlite/tokio, and `qfs-store` is the crate that owns a real `rusqlite::Connection`. So
//! the consumer-side trait is defined up in the domain leaf and its SQLite implementation is injected
//! here — the same split t45's `IdentityStore` / t46's `SessionStore` use. The
//! `qfs-store -> qfs-identity` edge is acyclic (qfs-identity depends on neither qfs-store nor
//! rusqlite).
//!
//! ## Token hygiene + atomic single-use (RFD §10)
//! The store trades only in token HASHES — the plaintext one-time token never reaches this crate.
//! `expires_at` is computed in SQL (`strftime(... '+N seconds')`) so the clock is the DB's.
//! [`SqliteInviteStore::accept_invite`] runs the WHOLE redeem inside ONE transaction: it fetches the
//! invite by its indexed `token_hash`, re-verifies the digest with a CONSTANT-TIME compare
//! ([`qfs_identity::InviteToken::matches_hash`]), refuses an expired / consumed / revoked invite,
//! creates the redeemer's local identity (`users` + a `local` `accounts` row, the t45 sign-up shape)
//! and the `memberships` row, then burns the invite with a GUARDED update
//! (`SET consumed_at = now WHERE id = ? AND consumed_at IS NULL`) — so two concurrent redeems of the
//! same token cannot both win. Any error rolls the transaction back: no half-created user, and a
//! still-valid invite is left unspent.

use std::sync::Mutex;

use qfs_identity::{
    Invite, InviteError, InviteId, InviteStore, Membership, MembershipId, MembershipScope,
    NewInvite, PasswordHash, Redemption, Role, User, UserId, UserStatus, PROVIDER_LOCAL,
};
use rusqlite::{Connection, ErrorCode, OptionalExtension};

use crate::Db;

/// The SQL fragment for "now" in the fixed-width RFC-3339-UTC form the schema stamps. Inlined into
/// each statement so expiry comparisons use the DB's clock, not the caller's.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%SZ','now')";

/// The System-DB-backed invite store. Owns the migrated connection inside a `Mutex` so the whole
/// backend is `Send + Sync` (mirrors `SqliteIdentityStore` / `SqliteSessionStore`). Holds no token
/// material.
pub struct SqliteInviteStore {
    conn: Mutex<Connection>,
}

impl SqliteInviteStore {
    /// Build the store over a migrated System-DB connection. The invites migration (v8) must already
    /// be applied — `SystemDb::open` does that on start.
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

    /// Lock the connection mutex, mapping a poisoned lock to a secret-free backend error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, InviteError> {
        self.conn
            .lock()
            .map_err(|_| InviteError::Backend("invite store lock poisoned".into()))
    }
}

/// Whether a rusqlite error is a UNIQUE/constraint violation (a duplicate), vs. any other failure.
fn is_constraint_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation
    )
}

/// The columns the invite row maps from (kept in one place so the SELECTs agree).
const INVITE_COLS: &str =
    "id, email, scope, project, role, created_by, created_at, expires_at, consumed_at, revoked_at";

/// Map a row of [`INVITE_COLS`] into an owned [`Invite`] (the `token_hash` is deliberately NOT
/// selected here — it never leaves the store on a DTO).
fn row_to_invite(r: &rusqlite::Row<'_>) -> rusqlite::Result<Invite> {
    Ok(Invite {
        id: InviteId(r.get(0)?),
        email: r.get(1)?,
        scope: MembershipScope::decode(&r.get::<_, String>(2)?),
        project: r.get(3)?,
        role: Role::decode(&r.get::<_, String>(4)?),
        created_by: r.get::<_, Option<i64>>(5)?.map(UserId),
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        consumed_at: r.get(8)?,
        revoked_at: r.get(9)?,
    })
}

/// Map a row `(id, user_id, scope, project, role, created_at)` into an owned [`Membership`].
fn row_to_membership(r: &rusqlite::Row<'_>) -> rusqlite::Result<Membership> {
    Ok(Membership {
        id: MembershipId(r.get(0)?),
        user_id: UserId(r.get(1)?),
        scope: MembershipScope::decode(&r.get::<_, String>(2)?),
        project: r.get(3)?,
        role: Role::decode(&r.get::<_, String>(4)?),
        created_at: r.get(5)?,
    })
}

impl InviteStore for SqliteInviteStore {
    fn create_invite(&self, new: &NewInvite, token_hash: &str) -> Result<Invite, InviteError> {
        let conn = self.lock()?;
        // `expires_at` = now + ttl, computed in SQL (the DB clock). A non-positive ttl yields an
        // already-expired invite (it can never redeem) rather than erroring.
        let modifier = format!("+{} seconds", new.ttl_secs);
        conn.execute(
            "INSERT INTO invites (token_hash, email, scope, project, role, created_by, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%SZ','now', ?7))",
            rusqlite::params![
                token_hash,
                new.email,
                new.scope.as_str(),
                new.project,
                new.role.as_str(),
                new.created_by.map(|u| u.0),
                modifier,
            ],
        )
        .map_err(|e| InviteError::Backend(format!("creating invite: {e}")))?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            &format!("SELECT {INVITE_COLS} FROM invites WHERE id = ?1"),
            rusqlite::params![id],
            row_to_invite,
        )
        .map_err(|e| InviteError::Backend(format!("reading back created invite: {e}")))
    }

    fn find_invite_by_token_hash(&self, token_hash: &str) -> Result<Option<Invite>, InviteError> {
        let conn = self.lock()?;
        conn.query_row(
            &format!("SELECT {INVITE_COLS} FROM invites WHERE token_hash = ?1"),
            rusqlite::params![token_hash],
            row_to_invite,
        )
        .optional()
        .map_err(|e| InviteError::Backend(format!("finding invite by token: {e}")))
    }

    fn find_invite(&self, id: InviteId) -> Result<Option<Invite>, InviteError> {
        let conn = self.lock()?;
        conn.query_row(
            &format!("SELECT {INVITE_COLS} FROM invites WHERE id = ?1"),
            rusqlite::params![id.0],
            row_to_invite,
        )
        .optional()
        .map_err(|e| InviteError::Backend(format!("finding invite: {e}")))
    }

    fn accept_invite(
        &self,
        token_hash: &str,
        email: &str,
        password_hash: &PasswordHash,
    ) -> Result<Redemption, InviteError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| InviteError::Backend(format!("starting redeem transaction: {e}")))?;

        // 1. Fetch the invite by its indexed token digest, including the lifecycle columns. We also
        // read back the stored `token_hash` so the verification routes through a CONSTANT-TIME
        // compare (defense-in-depth on top of the indexed equality fetch — RFD §10).
        type InviteRow = (
            i64,            // id
            String,         // token_hash
            String,         // scope
            String,         // role
            Option<String>, // project
            Option<String>, // consumed_at
            Option<String>, // revoked_at
        );
        let row: Option<InviteRow> = tx
            .query_row(
                "SELECT id, token_hash, scope, role, project, consumed_at, revoked_at \
                 FROM invites WHERE token_hash = ?1",
                rusqlite::params![token_hash],
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
            .map_err(|e| InviteError::Backend(format!("looking up invite: {e}")))?;
        let Some((invite_id, stored_hash, scope, role, project, consumed_at, revoked_at)) = row
        else {
            return Err(InviteError::NotFound);
        };
        // Constant-time digest verification: re-compare the stored digest against the presented
        // lookup key via the workspace's single constant-time primitive (RFD §10), defense-in-depth
        // on top of the indexed equality fetch. Both sides are sha256 digests, never the live token.
        if !qfs_crypto_core::constant_time_eq(stored_hash.as_bytes(), token_hash.as_bytes()) {
            return Err(InviteError::NotFound);
        }

        // 2. Lifecycle gates — precedence revoked → consumed → expired (mirrors `Invite::status_at`).
        if revoked_at.is_some() {
            return Err(InviteError::Revoked);
        }
        if consumed_at.is_some() {
            return Err(InviteError::AlreadyConsumed);
        }
        // Expiry on the DB clock: a row whose `expires_at` is at/before now is expired.
        let live: bool = tx
            .query_row(
                &format!("SELECT expires_at > {NOW_SQL} FROM invites WHERE id = ?1"),
                rusqlite::params![invite_id],
                |r| r.get(0),
            )
            .map_err(|e| InviteError::Backend(format!("checking invite expiry: {e}")))?;
        if !live {
            return Err(InviteError::Expired);
        }

        // 3. Create the redeemer's local identity — the t45 sign-up shape, inlined so it shares this
        // transaction (a duplicate email/account rolls the WHOLE redeem back, unspent).
        tx.execute(
            "INSERT INTO users (primary_email) VALUES (?1)",
            rusqlite::params![email],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                InviteError::DuplicateEmail
            } else {
                InviteError::Backend(format!("inserting user: {e}"))
            }
        })?;
        let user_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO accounts (user_id, provider, subject, password_hash) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![user_id, PROVIDER_LOCAL, email, password_hash.as_str()],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                InviteError::DuplicateAccount
            } else {
                InviteError::Backend(format!("inserting local account: {e}"))
            }
        })?;

        // 4. The membership row (links the new user to the invite's scope/project/role).
        tx.execute(
            "INSERT INTO memberships (user_id, scope, project, role, invite_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![user_id, scope, project, role, invite_id],
        )
        .map_err(|e| InviteError::Backend(format!("inserting membership: {e}")))?;
        let membership_id = tx.last_insert_rowid();

        // 5. Burn the invite — GUARDED so a concurrent redeem cannot also consume it. Exactly one row
        // must flip; zero means another redeem won the race (treat as already consumed).
        let burned = tx
            .execute(
                &format!(
                    "UPDATE invites SET consumed_at = {NOW_SQL} \
                     WHERE id = ?1 AND consumed_at IS NULL AND revoked_at IS NULL"
                ),
                rusqlite::params![invite_id],
            )
            .map_err(|e| InviteError::Backend(format!("burning the invite: {e}")))?;
        if burned != 1 {
            return Err(InviteError::AlreadyConsumed);
        }

        // 6. Read the created rows back BEFORE committing.
        let user = tx
            .query_row(
                "SELECT id, primary_email, created_at, status FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                |r| {
                    Ok(User {
                        id: UserId(r.get(0)?),
                        primary_email: r.get(1)?,
                        created_at: r.get(2)?,
                        status: UserStatus::decode(&r.get::<_, String>(3)?),
                    })
                },
            )
            .map_err(|e| InviteError::Backend(format!("reading back redeemed user: {e}")))?;
        let membership = tx
            .query_row(
                "SELECT id, user_id, scope, project, role, created_at FROM memberships WHERE id = ?1",
                rusqlite::params![membership_id],
                row_to_membership,
            )
            .map_err(|e| InviteError::Backend(format!("reading back membership: {e}")))?;

        tx.commit()
            .map_err(|e| InviteError::Backend(format!("committing redeem: {e}")))?;
        Ok(Redemption { user, membership })
    }

    fn revoke_invite(&self, id: InviteId) -> Result<bool, InviteError> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                &format!(
                    "UPDATE invites SET revoked_at = {NOW_SQL} \
                     WHERE id = ?1 AND consumed_at IS NULL AND revoked_at IS NULL"
                ),
                rusqlite::params![id.0],
            )
            .map_err(|e| InviteError::Backend(format!("revoking invite: {e}")))?;
        Ok(n > 0)
    }

    fn list_memberships(&self, user_id: UserId) -> Result<Vec<Membership>, InviteError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, user_id, scope, project, role, created_at \
                 FROM memberships WHERE user_id = ?1 ORDER BY id",
            )
            .map_err(|e| InviteError::Backend(format!("preparing memberships query: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![user_id.0], row_to_membership)
            .map_err(|e| InviteError::Backend(format!("reading memberships: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| InviteError::Backend(format!("reading membership row: {e}")))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{MemorySource, SystemDb};
    use qfs_identity::{hash_password, InviteStatus, InviteToken, Secret};

    fn store() -> SqliteInviteStore {
        SqliteInviteStore::from_db(SystemDb::open(&MemorySource).unwrap().into_db())
    }

    fn hash(pw: &str) -> PasswordHash {
        hash_password(&Secret::from(pw)).unwrap()
    }

    fn token(seed: &[u8]) -> InviteToken {
        InviteToken::from_entropy(seed)
    }

    fn member_invite(ttl_secs: i64) -> NewInvite {
        NewInvite {
            email: Some("invitee@x.io".into()),
            scope: MembershipScope::Host,
            project: None,
            role: Role::Member,
            ttl_secs,
            created_by: None,
        }
    }

    #[test]
    fn create_invite_stores_only_the_token_hash_not_the_plaintext() {
        let s = store();
        let t = token(&[1, 2, 3, 4]);
        let raw = t.reveal().expose_str().unwrap().to_string();
        let invite = s.create_invite(&member_invite(3600), &t.hash()).unwrap();
        assert_eq!(invite.email.as_deref(), Some("invitee@x.io"));
        assert_eq!(invite.role, Role::Member);
        // The DB column holds the sha256 digest, NEVER the raw token.
        let conn = s.lock().unwrap();
        let stored: String = conn
            .query_row(
                "SELECT token_hash FROM invites WHERE id = ?1",
                [invite.id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, t.hash());
        assert_ne!(stored, raw, "the raw token must never be stored");
        assert_eq!(stored.len(), 64, "a sha256_hex digest, not the token");
    }

    #[test]
    fn redeem_creates_an_identity_and_a_membership_then_burns_the_invite() {
        let s = store();
        let t = token(&[9, 9, 9, 9]);
        let invite = s.create_invite(&member_invite(3600), &t.hash()).unwrap();

        let redemption = s
            .accept_invite(&t.hash(), "new@x.io", &hash("password123"))
            .unwrap();
        assert_eq!(redemption.user.primary_email, "new@x.io");
        assert_eq!(redemption.membership.user_id, redemption.user.id);
        assert_eq!(redemption.membership.scope, MembershipScope::Host);
        assert_eq!(redemption.membership.role, Role::Member);

        // The user can now sign in (the local account carries the argon2id hash).
        let memberships = s.list_memberships(redemption.user.id).unwrap();
        assert_eq!(memberships.len(), 1);

        // The invite is burned (redeemed) — single use.
        let after = s.find_invite(invite.id).unwrap().unwrap();
        assert!(after.consumed_at.is_some());
        assert_eq!(
            after.status_at("2026-06-28T00:00:00Z"),
            InviteStatus::Redeemed
        );
    }

    #[test]
    fn a_redeemed_invite_is_rejected_on_replay() {
        let s = store();
        let t = token(&[1; 8]);
        s.create_invite(&member_invite(3600), &t.hash()).unwrap();
        s.accept_invite(&t.hash(), "first@x.io", &hash("password123"))
            .unwrap();
        // A second redeem of the SAME token is refused (single-use), and creates no second user.
        let err = s
            .accept_invite(&t.hash(), "second@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, InviteError::AlreadyConsumed);
        let conn = s.lock().unwrap();
        let users: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(users, 1, "a replay must not create a second user");
    }

    #[test]
    fn an_expired_invite_is_rejected() {
        let s = store();
        let t = token(&[2; 8]);
        // ttl 0 → expires_at == now → already expired.
        s.create_invite(&member_invite(0), &t.hash()).unwrap();
        let err = s
            .accept_invite(&t.hash(), "late@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, InviteError::Expired);
        // And no identity leaked out of the failed redeem (the transaction rolled back).
        let conn = s.lock().unwrap();
        let users: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(users, 0);
    }

    #[test]
    fn a_revoked_invite_is_rejected() {
        let s = store();
        let t = token(&[3; 8]);
        let invite = s.create_invite(&member_invite(3600), &t.hash()).unwrap();
        assert!(s.revoke_invite(invite.id).unwrap(), "first revoke succeeds");
        // Revoking again is idempotent (nothing left to revoke).
        assert!(!s.revoke_invite(invite.id).unwrap());
        let err = s
            .accept_invite(&t.hash(), "nope@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, InviteError::Revoked);
    }

    #[test]
    fn redeem_with_a_wrong_token_fails() {
        let s = store();
        let real = token(&[4; 8]);
        s.create_invite(&member_invite(3600), &real.hash()).unwrap();
        // A token that was never minted (different entropy) hashes to a digest with no invite.
        let forged = token(&[5; 8]);
        let err = s
            .accept_invite(&forged.hash(), "x@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, InviteError::NotFound);
    }

    #[test]
    fn redeem_into_an_existing_email_is_a_duplicate_and_leaves_the_invite_unspent() {
        let s = store();
        // Seed an existing user with the email the redeemer will try to use.
        {
            let conn = s.lock().unwrap();
            conn.execute(
                "INSERT INTO users (primary_email) VALUES ('taken@x.io')",
                [],
            )
            .unwrap();
        }
        let t = token(&[6; 8]);
        let invite = s.create_invite(&member_invite(3600), &t.hash()).unwrap();
        let err = s
            .accept_invite(&t.hash(), "taken@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, InviteError::DuplicateEmail);
        // The failed redeem rolled back — the invite is still pending and can be reused.
        let after = s.find_invite(invite.id).unwrap().unwrap();
        assert!(
            after.consumed_at.is_none(),
            "a failed redeem must not burn the invite"
        );
    }
}
