//! t45: the **rusqlite [`IdentityStore`] impl** over the System DB.
//!
//! This lives in `qfs-store` (not in `qfs-identity`) on purpose: `qfs-identity` is a pure-ish leaf
//! with NO rusqlite/tokio, and `qfs-store` is the crate that owns a real `rusqlite::Connection`. So
//! the consumer-side trait is defined up in the domain leaf and its SQLite implementation is injected
//! here — the same split t43 uses for the secret store (`SqliteSecrets` in the binary over the
//! Project DB). The `qfs-store -> qfs-identity` edge is acyclic (qfs-identity depends on neither).
//!
//! ## Transaction + hygiene
//! [`SqliteIdentityStore::signup_local`] inserts the `users` row + the `local` `accounts` row in ONE
//! transaction (a crash or a duplicate rolls back both — no half-created user). A UNIQUE violation
//! on `users.primary_email` → [`IdentityError::DuplicateEmail`]; on `accounts(provider, subject)` →
//! [`IdentityError::DuplicateAccount`]. The plaintext password never reaches this crate (the binary
//! hashes it, zeroizing the plaintext, and hands in a [`PasswordHash`]); the stored hash is read
//! internally only by [`SqliteIdentityStore::verify_password`] and is never returned or logged.

use std::sync::Mutex;

use qfs_identity::{
    verify_password as verify_against, Account, AccountId, IdentityError, IdentityStore,
    PasswordHash, Secret, SoleUser, User, UserId, UserStatus, PROVIDER_LOCAL,
};
use rusqlite::{Connection, ErrorCode, OptionalExtension};

use crate::Db;

/// The System-DB-backed identity store. Owns the migrated connection inside a `Mutex` so the whole
/// backend is `Send + Sync` (mirrors `SqliteSecrets`). Holds no key material.
pub struct SqliteIdentityStore {
    conn: Mutex<Connection>,
}

impl SqliteIdentityStore {
    /// Build the store over a migrated System-DB connection (obtain it via
    /// `SystemDb::into_db().into_connection()` in the binary). The identity migration (v3) must
    /// already be applied — `SystemDb::open` does that on start.
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
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, IdentityError> {
        self.conn
            .lock()
            .map_err(|_| IdentityError::Backend("identity store lock poisoned".into()))
    }

    /// **Register a member's per-recipient (E2E) PUBLIC key** on their `/sys/users` row (t80,
    /// decision U / §4.5). `public_key` is the publishable public half (e.g. hex-encoded uncompressed
    /// SEC1 P-256 bytes); the matching PRIVATE key NEVER touches the server — it stays client-side.
    /// This is the key a high-sensitivity connection's data-key is wrapped TO so only this member can
    /// unwrap it. Last-writer-wins (a member rotating their keypair re-registers). Not a secret: a
    /// public key is publishable metadata, so this is on the passphrase-free path.
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a DB failure (secret-free message). Updating an absent user
    /// affects zero rows and is still `Ok` (the caller asserts existence via [`Self::public_key`]).
    pub fn set_public_key(&self, user_id: UserId, public_key: &str) -> Result<(), IdentityError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE users SET public_key = ?1 WHERE id = ?2",
            rusqlite::params![public_key, user_id.0],
        )
        .map_err(|e| IdentityError::Backend(format!("registering member public key: {e}")))?;
        Ok(())
    }

    /// Read a member's registered per-recipient (E2E) PUBLIC key by email, or `None` if the user does
    /// not exist or has not registered a key (t80). Returns publishable metadata only — never a secret
    /// (the private key is not on the server). Used to wrap a high-sensitivity connection's DEK to an
    /// authorized member.
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a DB failure (secret-free message).
    pub fn public_key(&self, email: &str) -> Result<Option<String>, IdentityError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT public_key FROM users WHERE primary_email = ?1",
            rusqlite::params![email],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(|e| IdentityError::Backend(format!("reading member public key: {e}")))
    }
}

/// Whether a rusqlite error is a UNIQUE/constraint violation (a duplicate), vs. any other failure.
fn is_constraint_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation
    )
}

/// Map a row into an owned [`User`].
fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: UserId(r.get(0)?),
        primary_email: r.get(1)?,
        created_at: r.get(2)?,
        status: UserStatus::decode(&r.get::<_, String>(3)?),
    })
}

/// Map a row into an owned [`Account`] (the `password_hash` column is deliberately NOT selected —
/// it never leaves the store).
fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: AccountId(r.get(0)?),
        user_id: UserId(r.get(1)?),
        provider: r.get(2)?,
        subject: r.get(3)?,
        created_at: r.get(4)?,
    })
}

impl IdentityStore for SqliteIdentityStore {
    fn create_user(&self, email: &str) -> Result<User, IdentityError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO users (primary_email) VALUES (?1)",
            rusqlite::params![email],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                IdentityError::DuplicateEmail
            } else {
                IdentityError::Backend(format!("creating user: {e}"))
            }
        })?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            "SELECT id, primary_email, created_at, status FROM users WHERE id = ?1",
            rusqlite::params![id],
            row_to_user,
        )
        .map_err(|e| IdentityError::Backend(format!("reading back created user: {e}")))
    }

    fn find_user_by_email(&self, email: &str) -> Result<Option<User>, IdentityError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, primary_email, created_at, status FROM users WHERE primary_email = ?1",
            rusqlite::params![email],
            row_to_user,
        )
        .optional()
        .map_err(|e| IdentityError::Backend(format!("finding user by email: {e}")))
    }

    fn find_user_by_id(&self, id: UserId) -> Result<Option<User>, IdentityError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, primary_email, created_at, status FROM users WHERE id = ?1",
            rusqlite::params![id.0],
            row_to_user,
        )
        .optional()
        .map_err(|e| IdentityError::Backend(format!("finding user by id: {e}")))
    }

    fn create_account(
        &self,
        user_id: UserId,
        provider: &str,
        subject: &str,
        password_hash: Option<&PasswordHash>,
    ) -> Result<Account, IdentityError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO accounts (user_id, provider, subject, password_hash) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![user_id.0, provider, subject, password_hash.map(PasswordHash::as_str)],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                IdentityError::DuplicateAccount
            } else {
                IdentityError::Backend(format!("creating account: {e}"))
            }
        })?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            "SELECT id, user_id, provider, subject, created_at FROM accounts WHERE id = ?1",
            rusqlite::params![id],
            row_to_account,
        )
        .map_err(|e| IdentityError::Backend(format!("reading back created account: {e}")))
    }

    fn find_account(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<Account>, IdentityError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, user_id, provider, subject, created_at FROM accounts \
             WHERE provider = ?1 AND subject = ?2",
            rusqlite::params![provider, subject],
            row_to_account,
        )
        .optional()
        .map_err(|e| IdentityError::Backend(format!("finding account: {e}")))
    }

    fn verify_password(
        &self,
        provider: &str,
        subject: &str,
        candidate: &Secret,
    ) -> Result<bool, IdentityError> {
        let conn = self.lock()?;
        // `password_hash` is nullable: `Some(Some(h))` = a local account with a hash; `Some(None)` =
        // an account with no password (an OAuth account, M2); `None` = no such account.
        let stored: Option<Option<String>> = conn
            .query_row(
                "SELECT password_hash FROM accounts WHERE provider = ?1 AND subject = ?2",
                rusqlite::params![provider, subject],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| IdentityError::Backend(format!("reading password hash: {e}")))?;
        Ok(match stored {
            Some(Some(phc)) => verify_against(candidate, &PasswordHash::from_phc(phc)),
            // Account exists but has no password, or no account at all: not verifiable, not an error.
            Some(None) | None => false,
        })
    }

    fn signup_local(
        &self,
        email: &str,
        password_hash: &PasswordHash,
    ) -> Result<User, IdentityError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| IdentityError::Backend(format!("starting sign-up transaction: {e}")))?;

        // 1. The user row (unique email).
        tx.execute(
            "INSERT INTO users (primary_email) VALUES (?1)",
            rusqlite::params![email],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                IdentityError::DuplicateEmail
            } else {
                IdentityError::Backend(format!("inserting user: {e}"))
            }
        })?;
        let user_id = tx.last_insert_rowid();

        // 2. The local account (subject = email; unique on (provider, subject)).
        tx.execute(
            "INSERT INTO accounts (user_id, provider, subject, password_hash) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![user_id, PROVIDER_LOCAL, email, password_hash.as_str()],
        )
        .map_err(|e| {
            if is_constraint_violation(&e) {
                IdentityError::DuplicateAccount
            } else {
                IdentityError::Backend(format!("inserting local account: {e}"))
            }
        })?;

        // Read the created user back (for created_at/status) BEFORE committing, then commit.
        let user = tx
            .query_row(
                "SELECT id, primary_email, created_at, status FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                row_to_user,
            )
            .map_err(|e| IdentityError::Backend(format!("reading back signed-up user: {e}")))?;
        tx.commit()
            .map_err(|e| IdentityError::Backend(format!("committing sign-up: {e}")))?;
        Ok(user)
    }

    fn sole_user(&self) -> Result<SoleUser, IdentityError> {
        let conn = self.lock()?;
        // LIMIT 2 distinguishes none / exactly-one / many in a single read.
        let mut stmt = conn
            .prepare("SELECT id, primary_email, created_at, status FROM users ORDER BY id LIMIT 2")
            .map_err(|e| IdentityError::Backend(format!("preparing sole-user query: {e}")))?;
        let mut users = Vec::new();
        let rows = stmt
            .query_map([], row_to_user)
            .map_err(|e| IdentityError::Backend(format!("reading users: {e}")))?;
        for r in rows {
            users.push(r.map_err(|e| IdentityError::Backend(format!("reading user row: {e}")))?);
        }
        Ok(match users.len() {
            0 => SoleUser::None,
            1 => SoleUser::One(users.remove(0)),
            _ => SoleUser::Many,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::{MemorySource, SystemDb};

    fn store() -> SqliteIdentityStore {
        SqliteIdentityStore::from_db(SystemDb::open(&MemorySource).unwrap().into_db())
    }

    fn hash(pw: &str) -> PasswordHash {
        qfs_identity::hash_password(&Secret::from(pw)).unwrap()
    }

    #[test]
    fn signup_local_creates_a_user_and_a_local_account() {
        let s = store();
        let user = s.signup_local("a@b.com", &hash("password123")).unwrap();
        assert_eq!(user.primary_email, "a@b.com");
        assert_eq!(user.status, UserStatus::Active);
        assert!(!user.created_at.is_empty());

        // The user + a 'local' account (subject = email) now exist.
        assert_eq!(
            s.find_user_by_email("a@b.com").unwrap().unwrap().id,
            user.id
        );
        let acct = s.find_account(PROVIDER_LOCAL, "a@b.com").unwrap().unwrap();
        assert_eq!(acct.user_id, user.id);
        assert_eq!(acct.provider, "local");
    }

    #[test]
    fn member_public_key_registers_and_reads_back_and_is_absent_by_default() {
        // t80: a member registers their per-recipient (E2E) PUBLIC key; it is publishable metadata,
        // read back by email. Absent until registered; last-writer-wins on a keypair rotation.
        let s = store();
        let user = s.signup_local("m@team.io", &hash("password123")).unwrap();
        // No key registered yet ⇒ None (the connection cannot wrap to this member yet).
        assert_eq!(s.public_key("m@team.io").unwrap(), None);

        s.set_public_key(user.id, "04AABBCC_pubkey_hex").unwrap();
        assert_eq!(
            s.public_key("m@team.io").unwrap().as_deref(),
            Some("04AABBCC_pubkey_hex")
        );

        // Re-registering (keypair rotation) overwrites — last-writer-wins.
        s.set_public_key(user.id, "04DDEEFF_rotated").unwrap();
        assert_eq!(
            s.public_key("m@team.io").unwrap().as_deref(),
            Some("04DDEEFF_rotated")
        );

        // An unknown member has no key (and reading it is a plain None, not an error).
        assert_eq!(s.public_key("ghost@team.io").unwrap(), None);
    }

    #[test]
    fn signup_password_verifies_and_wrong_one_does_not() {
        let s = store();
        s.signup_local("u@x.io", &hash("hunter2hunter2")).unwrap();
        assert!(s
            .verify_password(PROVIDER_LOCAL, "u@x.io", &Secret::from("hunter2hunter2"))
            .unwrap());
        assert!(!s
            .verify_password(PROVIDER_LOCAL, "u@x.io", &Secret::from("wrong"))
            .unwrap());
        // An unknown subject is a plain false, not an error.
        assert!(!s
            .verify_password(
                PROVIDER_LOCAL,
                "nobody@x.io",
                &Secret::from("hunter2hunter2")
            )
            .unwrap());
    }

    #[test]
    fn duplicate_email_is_a_structured_error_and_rolls_back() {
        let s = store();
        s.signup_local("dup@x.io", &hash("password123")).unwrap();
        let err = s
            .signup_local("dup@x.io", &hash("password123"))
            .unwrap_err();
        assert_eq!(err, IdentityError::DuplicateEmail);
        // The rollback means NO orphan second user/account was created.
        let conn = s.lock().unwrap();
        let users: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        let accounts: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            users, 1,
            "the duplicate sign-up must not create a second user"
        );
        assert_eq!(accounts, 1, "and must not create a second account");
    }

    #[test]
    fn the_password_hash_is_never_returned_on_an_account_dto() {
        // The Account DTO has no hash field; selecting it back never carries the secret.
        let s = store();
        s.signup_local("h@x.io", &hash("password123")).unwrap();
        let acct = s.find_account(PROVIDER_LOCAL, "h@x.io").unwrap().unwrap();
        let dumped = format!("{acct:?}");
        assert!(
            !dumped.contains("argon2"),
            "no hash material may appear in an Account dump: {dumped}"
        );
    }

    #[test]
    fn oauth_style_account_without_a_password_cannot_verify() {
        let s = store();
        let user = s.create_user("oauth@x.io").unwrap();
        // A provider account with NO password hash (the M2 OAuth shape).
        s.create_account(user.id, "google", "google-subject-123", None)
            .unwrap();
        assert!(!s
            .verify_password("google", "google-subject-123", &Secret::from("anything"))
            .unwrap());
    }

    #[test]
    fn sole_user_resolves_none_one_then_many() {
        let s = store();
        assert_eq!(s.sole_user().unwrap(), SoleUser::None);
        s.signup_local("one@x.io", &hash("password123")).unwrap();
        match s.sole_user().unwrap() {
            SoleUser::One(u) => assert_eq!(u.primary_email, "one@x.io"),
            other => panic!("expected exactly one user, got {other:?}"),
        }
        s.signup_local("two@x.io", &hash("password123")).unwrap();
        assert_eq!(s.sole_user().unwrap(), SoleUser::Many);
    }
}
