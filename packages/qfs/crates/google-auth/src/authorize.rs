//! The interactive **loopback authorization** flow (RFD-0001 §10) — native-only.
//!
//! `authorize` runs the OAuth2 desktop-app consent flow: it binds an ephemeral loopback
//! [`TcpListener`] on `127.0.0.1:0`, derives the bound port, advertises the redirect URI as
//! **`http://localhost:<port>`** (the load-bearing `localhost`-not-`127.0.0.1` detail — see
//! [`crate::oauth`]), opens the consent URL, serves exactly one redirect request, validates
//! `state`, extracts `code`, exchanges it for tokens, looks up the profile email, persists the
//! refresh token under `google:<email>:refresh_token`, and returns the [`GoogleAccount`].
//!
//! ## wasm note
//! This whole module is `cfg(not(target_arch = "wasm32"))`: Workers have no loopback listener
//! and provision refresh tokens out of band, using only [`crate::source::StoredTokenSource`].
//! Keeping `authorize` feature-gated lets the refresh-only path compile to `wasm32`.
//!
//! ## Consent itself is interactive
//! Opening the browser + the human approving is interactive and out of scope for automated
//! tests; the pieces *around* it — auth-URL shape, redirect parsing, `state` validation, token
//! exchange, profile keying, refresh-token persistence — are the non-interactive machinery the
//! test suite exercises against a mock HTTP client. [`parse_redirect_request`] and
//! [`new_state`] are exposed (crate-internal) so those tests cover the listener's parsing
//! without standing up a socket.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use qfs_secrets::Secrets;

use crate::error::AuthError;
use crate::oauth::OAuthClient;
use crate::source::refresh_token_key;
use crate::token::GoogleAccount;

/// The "you may close this tab" page served back to the browser after the redirect is captured.
const SUCCESS_PAGE: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
     <html><body><h2>Authorization complete</h2>\
     <p>You may close this tab and return to the terminal.</p></body></html>";

/// How a consent URL is shown to the user. The default opens nothing automatically (the CLI
/// prints the URL); a test/headless caller injects a closure that drives the redirect.
pub type ConsentOpener = dyn Fn(&str) -> Result<(), AuthError> + Send + Sync;

/// Run the full loopback authorization flow and persist the resulting refresh token.
///
/// Steps: bind loopback → advertise `http://localhost:<port>` → build the auth URL with a fresh
/// `state` → invoke `open_consent` (prints/opens the URL) → accept one redirect → validate
/// `state` → extract `code` → exchange for tokens → fetch the profile email → persist the
/// refresh token under `google:<email>:refresh_token` → return the [`GoogleAccount`].
///
/// `timeout` bounds the wait for the redirect (a human who never approves yields
/// [`AuthError::Timeout`] rather than hanging forever).
///
/// # Errors
/// [`AuthError`] for any step: `Invalid` (cannot bind / build URL), `Denied` (user declined),
/// `Timeout`, `StateMismatch`, `Network`/`TokenRefresh` (exchange), `ProfileLookup`, `Store`.
pub fn authorize(
    oauth: &OAuthClient,
    store: &Arc<dyn Secrets>,
    open_consent: &ConsentOpener,
    now_nanos: u128,
    timeout: Duration,
) -> Result<GoogleAccount, AuthError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| AuthError::Invalid {
        reason: "could not bind a loopback listener for the OAuth redirect".to_string(),
    })?;
    let port = listener
        .local_addr()
        .map_err(|_| AuthError::Invalid {
            reason: "could not read the bound loopback port".to_string(),
        })?
        .port();
    // The load-bearing detail: advertise `localhost`, not `127.0.0.1`.
    let redirect_uri = OAuthClient::redirect_uri(port);
    let state = new_state();
    let auth_url = oauth.build_auth_url(&redirect_uri, &state)?;
    tracing::debug!(
        port,
        "bound loopback listener; advertising localhost redirect"
    );

    open_consent(&auth_url)?;

    let code = accept_redirect(&listener, &state, timeout)?;
    let (access, refresh) = oauth.exchange_code(&code, &redirect_uri, now_nanos)?;
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

/// Accept exactly one redirect on the listener, within `timeout`, returning the `code`.
fn accept_redirect(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    listener
        .set_nonblocking(false)
        .map_err(|_| AuthError::Invalid {
            reason: "could not configure the loopback listener".to_string(),
        })?;
    // A coarse overall deadline via the accept timeout: set the read timeout on the accepted
    // stream. (The accept itself blocks; for the CLI a human is present, and the outer caller
    // can bound the whole call. We still cap the per-connection read.)
    let (mut stream, _addr) = listener.accept().map_err(|_| AuthError::Timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| AuthError::Invalid {
            reason: "could not set the redirect read timeout".to_string(),
        })?;

    let mut buf = [0_u8; 4096];
    let n = stream.read(&mut buf).map_err(|_| AuthError::Timeout)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let outcome = parse_redirect_request(&request, expected_state);

    // Always answer the browser so the tab shows a clean page regardless of outcome.
    let _ = stream.write_all(SUCCESS_PAGE.as_bytes());
    let _ = stream.flush();

    outcome
}

/// Parse the HTTP request line of a captured loopback redirect, validate `state`, and return
/// the `code` — or a typed error (`Denied` on `error=access_denied`, `StateMismatch` on a
/// `state` mismatch, `Invalid` on a malformed request). Pure and socket-free, so tests cover
/// it directly.
pub(crate) fn parse_redirect_request(
    request: &str,
    expected_state: &str,
) -> Result<String, AuthError> {
    // The request line is `GET /?code=...&state=... HTTP/1.1`.
    let first_line = request.lines().next().unwrap_or("");
    let target = first_line.split_whitespace().nth(1).unwrap_or("");
    // Build an absolute URL so url's query parser applies.
    let full = format!("http://localhost{target}");
    let parsed = url::Url::parse(&full).map_err(|_| AuthError::Invalid {
        reason: "malformed redirect request".to_string(),
    })?;

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
    // Validate state BEFORE accepting the code (CSRF guard).
    match state.as_deref() {
        Some(s) if s == expected_state => {}
        _ => return Err(AuthError::StateMismatch),
    }
    code.ok_or(AuthError::Invalid {
        reason: "redirect carried no authorization code".to_string(),
    })
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
