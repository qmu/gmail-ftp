//! The pure request-authentication step: a `Cookie` header → an optional authenticated [`UserId`].
//!
//! This is the read-side plumbing the HTTP listener performs per request (t46 step 5): read the
//! `Cookie` header, extract the session token, hash it, look the hash up in the [`SessionStore`],
//! and yield the bound [`UserId`] if a live session exists. It is a PURE function over an injected
//! store, so it is unit-testable without the tokio listener.
//!
//! **It grants NOTHING.** Resolving a `UserId` here is authentication STATE only — no path may
//! refuse an unauthenticated request (t50/t51) or authorize a request (M2) on the strength of it
//! this milestone. The result is deliberately inert.

use crate::{parse_cookie_header, token_hash, SessionError, SessionStore, UserId};

/// Resolve the authenticated [`UserId`] for a request from its `Cookie` header value, if any.
///
/// - `cookie_header`: the raw `Cookie` request-header value (`None` when the request carries no
///   `Cookie` header).
/// - Returns `Ok(Some(user_id))` iff the header carries a `qfs_session` token whose hash resolves to
///   a LIVE (non-expired) session; `Ok(None)` when there is no cookie, no `qfs_session` value, or no
///   live session for it. An expired/absent session is `Ok(None)`, never an error.
///
/// The raw token never leaves this function — it is hashed ([`token_hash`]) before the lookup, so
/// the store only ever sees the hash.
///
/// # Errors
/// [`SessionError::Backend`] if the store read itself fails (distinct from "not authenticated",
/// which is a plain `Ok(None)`).
pub fn authenticate(
    cookie_header: Option<&str>,
    store: &dyn SessionStore,
) -> Result<Option<UserId>, SessionError> {
    let Some(header) = cookie_header else {
        return Ok(None);
    };
    let Some(token) = parse_cookie_header(header) else {
        return Ok(None);
    };
    let hash = token_hash(&token);
    Ok(store.lookup(&hash)?.map(|session| session.user_id))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{Session, SessionId};

    /// A tiny in-memory `SessionStore` for the pure-authenticate tests (the real impl is the
    /// injected rusqlite store in qfs-store). Maps `token_hash -> (user_id, expired?)`.
    #[derive(Default)]
    struct MockStore {
        rows: Mutex<Vec<(String, i64, bool)>>,
    }

    impl MockStore {
        fn insert(&self, token_hash: &str, user_id: i64, expired: bool) {
            self.rows
                .lock()
                .unwrap()
                .push((token_hash.to_string(), user_id, expired));
        }
    }

    impl SessionStore for MockStore {
        fn create(
            &self,
            _user_id: UserId,
            _token_hash: &str,
            _ttl_secs: i64,
        ) -> Result<Session, SessionError> {
            unreachable!("not exercised by the authenticate tests")
        }

        fn lookup(&self, token_hash: &str) -> Result<Option<Session>, SessionError> {
            let rows = self.rows.lock().unwrap();
            for (h, uid, expired) in rows.iter() {
                if h == token_hash && !*expired {
                    return Ok(Some(Session {
                        id: SessionId(h.clone()),
                        user_id: UserId(*uid),
                        created_at: "2026-06-28T00:00:00Z".into(),
                        expires_at: "2999-01-01T00:00:00Z".into(),
                        rotated_from: None,
                    }));
                }
            }
            Ok(None)
        }

        fn rotate(&self, _old: &str, _new: &str, _ttl: i64) -> Result<Session, SessionError> {
            unreachable!()
        }

        fn revoke(&self, _token_hash: &str) -> Result<bool, SessionError> {
            unreachable!()
        }
    }

    #[test]
    fn no_cookie_header_is_unauthenticated() {
        let store = MockStore::default();
        assert_eq!(authenticate(None, &store).unwrap(), None);
    }

    #[test]
    fn cookie_without_the_session_pair_is_unauthenticated() {
        let store = MockStore::default();
        assert_eq!(
            authenticate(Some("theme=dark; lang=en"), &store).unwrap(),
            None
        );
    }

    #[test]
    fn a_live_session_resolves_to_its_user() {
        let store = MockStore::default();
        // The cookie carries the raw token "tok"; the store is keyed by its hash.
        store.insert(&token_hash("tok"), 7, false);
        assert_eq!(
            authenticate(Some("qfs_session=tok"), &store).unwrap(),
            Some(UserId(7))
        );
    }

    #[test]
    fn an_expired_or_unknown_token_is_unauthenticated_not_an_error() {
        let store = MockStore::default();
        store.insert(&token_hash("stale"), 3, true); // expired
        assert_eq!(
            authenticate(Some("qfs_session=stale"), &store).unwrap(),
            None
        );
        assert_eq!(
            authenticate(Some("qfs_session=never-issued"), &store).unwrap(),
            None
        );
    }
}
