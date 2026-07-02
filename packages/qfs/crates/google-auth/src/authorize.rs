//! The interactive **paste-back authorization** flow (RFD-0001 §10) — native-only.
//!
//! `authorize` runs the OAuth2 desktop-app consent flow entirely over the terminal, the way
//! gmail-ftp does: build the consent URL with `redirect_uri=http://localhost` (no port — and
//! **no listener**), show it to the human, let them approve in **any** browser (their laptop's,
//! over SSH — the redirect never needs to reach this host), then read back the
//! `http://localhost/?state=…&code=…` URL their browser lands on (or the bare `code=` value),
//! validate `state`, exchange the code for tokens, look up the profile email, persist the
//! refresh token under `google:<email>:refresh_token`, and return the [`GoogleAccount`].
//!
//! ## Why paste-back, not a loopback listener
//! A loopback `TcpListener` can only receive the redirect when the approving browser runs on
//! THIS host. The primary qfs environment is SSH-to-a-server: the browser is on the user's own
//! machine, its `http://localhost/...` redirect lands on *their* loopback, and no listener here
//! ever sees it. Paste-back works in both worlds — Google checks that the token request's
//! `redirect_uri` EQUALS the consent URL's, not that anything answered at it — so it is THE
//! flow, with no listener variant to fall back to.
//!
//! ## wasm note
//! This whole module is `cfg(not(target_arch = "wasm32"))`: Workers provision refresh tokens
//! out of band, using only [`crate::source::StoredTokenSource`]. Keeping `authorize`
//! feature-gated lets the refresh-only path compile to `wasm32`.
//!
//! ## Consent itself is interactive
//! Approving in a browser and pasting the redirect back is a human act, out of scope for
//! automated tests; everything *around* it — auth-URL shape, pasted-input parsing, `state`
//! validation, token exchange, profile keying, refresh-token persistence — is exercised
//! hermetically by injecting a [`ConsentPrompt`] that answers with a scripted paste.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use qfs_secrets::Secrets;

use crate::error::AuthError;
use crate::oauth::{OAuthClient, PASTE_REDIRECT_URI};
use crate::source::refresh_token_key;
use crate::token::GoogleAccount;

/// The interaction seam: present the consent URL to the human and return the line they paste
/// back — the full `http://localhost/?state=…&code=…` redirect URL or the bare `code=` value.
/// The CLI implements this over the controlling terminal; a test injects a scripted answer.
pub type ConsentPrompt = dyn Fn(&str) -> Result<String, AuthError> + Send + Sync;

/// Run the full paste-back authorization flow and persist the resulting refresh token.
///
/// Steps: build the auth URL with `redirect_uri=http://localhost` + a fresh `state` → invoke
/// `prompt` (shows the URL, returns the pasted redirect) → validate `state` → extract `code` →
/// exchange for tokens → fetch the profile email → persist the refresh token under
/// `google:<email>:refresh_token` → return the [`GoogleAccount`].
///
/// There is no timeout: the flow blocks on the human's paste, exactly like any terminal prompt;
/// interrupting the process abandons the single-use `state` harmlessly.
///
/// # Errors
/// [`AuthError`] for any step: `Invalid` (cannot build the URL / nothing pasted), `Denied`
/// (user declined), `StateMismatch`, `Network`/`TokenRefresh` (exchange), `ProfileLookup`,
/// `Store`.
pub fn authorize(
    oauth: &OAuthClient,
    store: &Arc<dyn Secrets>,
    prompt: &ConsentPrompt,
    now_nanos: u128,
) -> Result<GoogleAccount, AuthError> {
    let state = new_state();
    let auth_url = oauth.build_auth_url(PASTE_REDIRECT_URI, &state)?;

    let pasted = prompt(&auth_url)?;
    let code = code_from_pasted_redirect(&pasted, &state)?;

    // The token request must carry the SAME redirect_uri as the consent URL — Google checks
    // equality, not reachability (nothing ever listened at http://localhost).
    let (access, refresh) = oauth.exchange_code(&code, PASTE_REDIRECT_URI, now_nanos)?;
    let email = oauth.fetch_profile_email(&access)?;

    // Persist the refresh token under google:<email>:refresh_token. The Secret moves into the
    // store; it is never logged. The email (non-secret) is the account key.
    let key = refresh_token_key(&email)?;
    store.put(&key, refresh).map_err(AuthError::from)?;
    tracing::debug!(account = %email, "persisted refresh token for account");

    // Re-load is unnecessary; we return the account with a fresh Secret for the caller's use.
    // The stored copy is authoritative for later StoredTokenSource refreshes.
    let stored = store.get(&key).map_err(AuthError::from)?;
    Ok(GoogleAccount::new(email, stored))
}

/// Extract the authorization `code` from the line the user pasted back: the full redirect URL
/// (`http://localhost/?state=…&code=…`), validated against the expected `state` (CSRF guard)
/// and surfacing an `error=` denial — or, when the input does not parse as a URL carrying a
/// code or an error, the trimmed input taken as the bare code. Pure, so tests cover it
/// directly.
pub(crate) fn code_from_pasted_redirect(
    input: &str,
    expected_state: &str,
) -> Result<String, AuthError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AuthError::Invalid {
            reason: "nothing was pasted — expected the redirect URL or the code= value".to_string(),
        });
    }
    if let Ok(parsed) = url::Url::parse(trimmed) {
        let mut code: Option<String> = None;
        let mut state: Option<String> = None;
        let mut error: Option<String> = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(err) = error {
            // `access_denied` is the user declining; anything else is a protocol-level rejection.
            if err == "access_denied" {
                return Err(AuthError::Denied);
            }
            return Err(AuthError::TokenRefresh { reason: err });
        }
        if let Some(code) = code {
            // Validate state BEFORE accepting the code (CSRF guard) — a URL-shaped paste that
            // carries a code but a wrong/missing state is rejected, never exchanged.
            match state.as_deref() {
                Some(s) if s == expected_state => return Ok(code),
                _ => return Err(AuthError::StateMismatch),
            }
        }
    }
    // Not a URL carrying a code or an error: the user pasted the bare code value.
    Ok(trimmed.to_string())
}

/// Generate an unguessable `state` value (CSRF token) for one authorize attempt. Uses a
/// process-unique, time + address-seeded value; it is single-use and validated on return, so it
/// does not need cryptographic strength here — only unpredictability across concurrent flows.
pub(crate) fn new_state() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded per-process from the OS; hashing a fresh allocation's address +
    // the current instant yields a value an attacker on the loopback cannot predict.
    let mut h = RandomState::new().build_hasher();
    let marker = Box::new(0_u8);
    h.write_usize(std::ptr::from_ref::<u8>(&*marker) as usize);
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}", h.finish())
}
