//! [`TokenSource`] — the reusable bearer-token provider the Gmail (t20), Drive (t21), and
//! Analytics (t41) drivers depend on — and [`StoredTokenSource`], its store-backed
//! implementation (RFD-0001 §5/§10).
//!
//! A consuming driver holds a `TokenSource` and calls [`TokenSource::access_token`] at
//! request-build time to get a fresh bearer value; the refresh is **transparent** behind the
//! trait. `StoredTokenSource` loads the per-account refresh token from the t27 store
//! (`google:<email>:refresh_token`), mints an access token via [`OAuthClient`], caches it
//! until just before expiry, and refreshes on a cache miss.
//!
//! ## Synchronous by design
//! `access_token` is **synchronous**, matching the t18 [`qfs_driver_http::HttpClient`] seam and
//! the synchronous-applier discipline of every qfs driver (the runtime bridge offloads the
//! apply leg to a tokio blocking thread, so a blocking refresh here never stalls a runtime
//! worker, and no async runtime leaks into this crate or the spine). The ticket's `async fn`
//! intent is satisfied at the apply-leg boundary, not by making this crate async.

use std::sync::{Arc, Mutex};

use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secret, Secrets};

use crate::error::AuthError;
use crate::oauth::OAuthClient;
use crate::token::{AccessToken, Clock, SystemClock};

/// The credential-store driver id under which Google refresh tokens are namespaced. Every
/// account is keyed `(google, <email>)` so all Google drivers (gmail/drive/analytics) share one
/// account namespace — a single consent serves them all.
pub const GOOGLE_DRIVER_ID: &str = "google";

/// Encode a Google profile **email** into a t27-valid [`ConnectionId`]. The t27 [`ConnectionId`]
/// deliberately forbids `@`, `/`, and whitespace (they collide with the `@account` selector and
/// the `driver/account` store-key encoding), but a Google account *is* an email containing `@`.
/// We percent-style escape exactly those three classes — using `%` (an allowed char) as the
/// escape — yielding an injective, reversible encoding: `alice@example.com` →
/// `alice%40example.com`. Distinct emails always map to distinct account ids (no collision),
/// and [`decode_account_email`] recovers the original.
fn encode_account_email(email: &str) -> String {
    let mut out = String::with_capacity(email.len());
    for c in email.chars() {
        match c {
            '%' => out.push_str("%25"),
            '@' => out.push_str("%40"),
            '/' => out.push_str("%2f"),
            c if c.is_whitespace() => {
                for b in c.to_string().bytes() {
                    out.push_str(&format!("%{b:02x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Recover the original email from an [`encode_account_email`] result (the inverse). Used to
/// list/round-trip stored Google accounts back to their display emails.
#[must_use]
pub fn decode_account_email(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `(google, <encoded-email>)` [`CredentialKey`] used for storage/retrieval. The account
/// email is encoded into a t27-valid [`ConnectionId`] (see [`encode_account_email`]); the stored
/// *value* is the refresh token.
///
/// # Errors
/// [`AuthError::Invalid`] if `email` is empty (the encoding cannot produce a valid, non-empty
/// account id).
pub fn refresh_token_key(email: &str) -> Result<CredentialKey, AuthError> {
    if email.is_empty() {
        return Err(AuthError::Invalid {
            reason: "account email is empty".to_string(),
        });
    }
    let encoded = encode_account_email(email);
    let account = ConnectionId::new(encoded).map_err(|e| AuthError::Invalid {
        reason: format!("account email could not be encoded to an account id: {e}"),
    })?;
    Ok(CredentialKey::new(DriverId::new(GOOGLE_DRIVER_ID), account))
}

/// The reusable bearer-token provider. A consuming driver depends on `&dyn TokenSource` (or an
/// `Arc<dyn TokenSource>`), never on the concrete [`StoredTokenSource`], so a Worker build can
/// swap in an out-of-band token source without touching driver code.
pub trait TokenSource: Send + Sync {
    /// Return a currently-valid access token, refreshing transparently if the cached one has
    /// expired. The returned token's `bearer()` is injected into an `Authorization: Bearer`
    /// header.
    ///
    /// # Errors
    /// [`AuthError`] — `TokenRefresh { reason: "invalid_grant" }` if the refresh token is
    /// revoked (re-authorize required), `Store` if the refresh token cannot be read, `Network`
    /// on transport failure.
    fn access_token(&self) -> Result<BorrowedToken<'_>, AuthError>;

    /// Force the next [`Self::access_token`] to refresh — called by the authenticated client
    /// after a 401, so an access token that the server rejected (despite a not-yet-elapsed
    /// local expiry) is discarded and re-minted exactly once.
    fn invalidate(&self);
}

/// A guard handing out the cached access token's bearer value without cloning the [`Secret`].
/// Holds the cache lock for its lifetime, so callers `bearer()` it, build the request, and drop
/// it promptly. Kept tiny so the lock is held only across header construction.
pub struct BorrowedToken<'a> {
    guard: std::sync::MutexGuard<'a, Option<AccessToken>>,
}

impl BorrowedToken<'_> {
    /// The bearer value to inject into an `Authorization: Bearer <token>` header. `None` only
    /// in the impossible case of a non-UTF-8 access token.
    #[must_use]
    pub fn bearer(&self) -> Option<&str> {
        self.guard.as_ref().and_then(AccessToken::bearer)
    }
}

/// Redacting `Debug`: surfaces only whether a token is present + its (redacted) `AccessToken`,
/// never the bearer value. Safe to drop into a log line or a `Result::unwrap_err` message.
impl core::fmt::Debug for BorrowedToken<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BorrowedToken")
            .field("token", &*self.guard)
            .finish()
    }
}

/// The store-backed [`TokenSource`]. Loads the refresh token for `account_email` from the t27
/// store, mints + caches an access token via `oauth`, and refreshes on expiry / [`invalidate`].
///
/// Multi-account: construct one `StoredTokenSource` per Google account email; each resolves its
/// own `google:<email>:refresh_token` and caches its own access token independently.
///
/// [`invalidate`]: TokenSource::invalidate
pub struct StoredTokenSource {
    account_email: String,
    store: Arc<dyn Secrets>,
    oauth: OAuthClient,
    clock: Arc<dyn Clock>,
    cached: Mutex<Option<AccessToken>>,
}

impl StoredTokenSource {
    /// Build a token source for `account_email`, reading the refresh token from `store` and
    /// minting access tokens via `oauth`. Uses the production [`SystemClock`].
    #[must_use]
    pub fn new(
        account_email: impl Into<String>,
        store: Arc<dyn Secrets>,
        oauth: OAuthClient,
    ) -> Self {
        Self::with_clock(account_email, store, oauth, Arc::new(SystemClock::new()))
    }

    /// Build with an injected [`Clock`] (the test seam for deterministic expiry).
    #[must_use]
    pub fn with_clock(
        account_email: impl Into<String>,
        store: Arc<dyn Secrets>,
        oauth: OAuthClient,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            account_email: account_email.into(),
            store,
            oauth,
            clock,
            cached: Mutex::new(None),
        }
    }

    /// The account email this source serves (the multi-account key). Non-secret.
    #[must_use]
    pub fn account_email(&self) -> &str {
        &self.account_email
    }

    /// Load this account's refresh token from the store. The returned [`Secret`] is moved
    /// straight into the refresh call; it is never logged.
    fn load_refresh_token(&self) -> Result<Secret, AuthError> {
        let key = refresh_token_key(&self.account_email)?;
        self.store.get(&key).map_err(AuthError::from)
    }

    /// Refresh the access token from the stored refresh token and replace the cache. Returns a
    /// borrowed handle to the freshly-minted token.
    fn refresh_into_cache(&self) -> Result<(), AuthError> {
        let refresh = self.load_refresh_token()?;
        let now = self.clock.now_nanos();
        let token = self.oauth.refresh_access_token(&refresh, now)?;
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some(token);
        }
        tracing::debug!(account = %self.account_email, "minted access token for account");
        Ok(())
    }
}

impl TokenSource for StoredTokenSource {
    fn access_token(&self) -> Result<BorrowedToken<'_>, AuthError> {
        // Fast path: a cached token that is still valid on our clock.
        {
            let guard = self.cached.lock().map_err(|_| AuthError::Invalid {
                reason: "token cache lock poisoned".to_string(),
            })?;
            if let Some(tok) = guard.as_ref() {
                if !tok.is_expired(self.clock.as_ref()) {
                    return Ok(BorrowedToken { guard });
                }
            }
        }
        // Miss or expired: refresh, then hand back the refreshed token.
        self.refresh_into_cache()?;
        let guard = self.cached.lock().map_err(|_| AuthError::Invalid {
            reason: "token cache lock poisoned".to_string(),
        })?;
        Ok(BorrowedToken { guard })
    }

    fn invalidate(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }
}
