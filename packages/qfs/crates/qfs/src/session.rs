//! The `qfs` session composition root (t46): the System-DB-backed [`SqliteSessionStore`] I/O plus
//! the binary-owned token entropy + cookie issuance, built ON the t45 identity store.
//!
//! `qfs-cmd` / the spine may not depend on the concrete `qfs-store` / `qfs-session` backends, so —
//! exactly like the identity launcher — the binary owns this. The binary is also the one crate that
//! resolves a real DB path (decision F) AND the one leaf that may own a CSPRNG: it mints the opaque
//! token from OS entropy here and hands `qfs-session` only the random bytes, keeping that core
//! deterministic/testable.
//!
//! ## Scope + security (decision §4.1, RFD §10)
//! - **AUTHENTICATION STATE ONLY.** A session proves WHO (a t45 `users` row), never WHAT-may-you-do.
//!   Authorization is M2; refusing unauthenticated requests is t50 (MCP) / t51 (SPA). So this is
//!   wired but **inert** — no endpoint is gated on a session this milestone (`serve` opens the store
//!   to prove it is ready, the same "wire without routing" posture t42 took for the System DB).
//! - **Token hygiene.** The raw token is generated here, returned to the caller exactly once inside
//!   the [`qfs_session::Secret`]-wrapped cookie value, and persisted ONLY as `sha256_hex(token)`.
//!   `Cookie`/`Set-Cookie` are in `qfs_http_core::SENSITIVE_HEADERS`, so the token is redacted in
//!   every log/trace line; the token is never logged or printed beyond the one-time issue.

use qfs_session::{
    format_set_cookie, Session, SessionStore, SessionToken, UserId, DEFAULT_SESSION_TTL_SECS,
};
use qfs_store::SqliteSessionStore;
use rand::RngCore;

/// The opaque token's entropy width in bytes (256 bits → a 64-char lowercase-hex token). 256 bits
/// of CSPRNG output is comfortably beyond brute-force/birthday reach for a bearer token.
const TOKEN_ENTROPY_BYTES: usize = 32;

/// Open the System DB at the default path and build the session store over its owned, migrated
/// connection (the t45/t46 seam — `SystemDb::into_db().into_connection()`). The session migration
/// (v4) is applied by `SystemDb::open`.
///
/// # Errors
/// A secret-free message if the System-DB path cannot be resolved (no `HOME`/`XDG_CONFIG_HOME`) or
/// the DB cannot be opened/migrated.
pub fn open_session_store() -> Result<SqliteSessionStore, String> {
    let sys = crate::store::open_system_db()
        .map_err(|e| format!("opening the system database: {e}"))?
        .ok_or("cannot determine the system database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(SqliteSessionStore::from_db(sys.into_db()))
}

/// Mint a fresh opaque session token from OS entropy (the binary owns the CSPRNG). The raw bytes are
/// handed to [`SessionToken::from_entropy`]; the returned token's plaintext lives only inside the
/// redacting [`qfs_session::Secret`] until it is rendered into the one-time cookie.
#[must_use]
pub fn generate_token() -> SessionToken {
    let mut entropy = [0u8; TOKEN_ENTROPY_BYTES];
    rand::rng().fill_bytes(&mut entropy);
    SessionToken::from_entropy(&entropy)
}

/// Issue a session for `user_id`: mint a token, persist ONLY its hash via `store.create`, and format
/// the `Set-Cookie` header value carrying the plaintext token (the one-time wire exposure). Returns
/// the created [`Session`] and the `Set-Cookie` value. `secure` gates the `Secure` attribute (set it
/// when the listener is trusted-HTTPS; leave it off for plain-localhost dev).
///
/// # Errors
/// A secret-free message if the store create fails (e.g. an unknown `user_id`).
pub fn issue_session(
    store: &dyn SessionStore,
    user_id: UserId,
    secure: bool,
) -> Result<(Session, String), String> {
    let token = generate_token();
    let session = store
        .create(user_id, &token.hash(), DEFAULT_SESSION_TTL_SECS)
        .map_err(|e| format!("issuing the session: {e}"))?;
    let set_cookie = format_set_cookie(&token, DEFAULT_SESSION_TTL_SECS, secure);
    Ok((session, set_cookie))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_session::{authenticate, parse_cookie_header};
    use qfs_store::{MemorySource, SystemDb};

    /// A session store over a fresh in-memory System DB holding one user (a session FKs to `users`).
    fn store_with_user() -> (SqliteSessionStore, UserId) {
        let sys = SystemDb::open(&MemorySource).unwrap();
        let conn = sys.into_db().into_connection();
        conn.execute("INSERT INTO users (primary_email) VALUES ('a@b.com')", [])
            .unwrap();
        let uid = UserId(conn.last_insert_rowid());
        (SqliteSessionStore::new(conn), uid)
    }

    #[test]
    fn generated_tokens_are_unique_and_64_hex_chars() {
        let a = generate_token();
        let b = generate_token();
        // 32 bytes → 64 lowercase-hex chars; CSPRNG output makes a collision negligible.
        assert_eq!(a.reveal().expose_str().unwrap().len(), 64);
        assert_ne!(a.hash(), b.hash(), "two minted tokens must differ");
    }

    #[test]
    fn issue_then_authenticate_round_trips_through_the_cookie() {
        let (store, uid) = store_with_user();
        let (_session, set_cookie) = issue_session(&store, uid, false).unwrap();

        // The browser echoes the cookie's name=value pair back on the next request.
        let pair = set_cookie.split(';').next().unwrap(); // "qfs_session=<token>"
        assert!(parse_cookie_header(pair).is_some());

        // The pure request-authentication step resolves the bound user from that Cookie header.
        let who = authenticate(Some(pair), &store).unwrap();
        assert_eq!(
            who,
            Some(uid),
            "the issued cookie authenticates as its user"
        );

        // A request with no cookie is unauthenticated (inert, but correct).
        assert_eq!(authenticate(None, &store).unwrap(), None);
    }
}
