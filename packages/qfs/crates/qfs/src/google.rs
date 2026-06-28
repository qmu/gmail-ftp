//! The **Google live-commit composition root** — the binary-side wiring that turns the already-built
//! `qfs-google-auth` machinery (the OAuth2 client, the per-account [`StoredTokenSource`], the
//! authenticated [`GoogleApiClient`]) plus the three Google driver clients
//! ([`GoogleApiGmailClient`](qfs_google_auth) etc.) into live apply drivers the commit registry
//! ([`crate::commit`]) can route `/mail`, `/drive`, and `/ga` legs through.
//!
//! ## Why this lives in the terminal binary
//! `qfs-google-auth` is a deliberately **runtime-free leaf**: it shares only the pure
//! `qfs-http-core` DTOs + `qfs-secrets`, never `reqwest` / `qfs-runtime` / `qfs-driver-http` (the
//! dep-direction guard pins this). The one real wire client (`reqwest`) lives confined in
//! `qfs-driver-http`; the binary bridges it onto the auth crate's thin `HttpExchange` seam in
//! [`crate::transport`]. So the composition that joins the wire transport, the OAuth app config, the
//! account-keyed token source, and the driver clients can only happen HERE, in the leaf where tokio
//! + reqwest dead-end — exactly like the github/slack composition in `commit.rs`.
//!
//! ## Fail-closed by construction (RFD §10)
//! There is **no baked-in OAuth app** — the operator registers a Google "Desktop" OAuth client and
//! supplies its id/secret via [`GOOGLE_CLIENT_ID_ENV`] / [`GOOGLE_CLIENT_SECRET_ENV`]. Absent either
//! (or absent a selected Google account email), [`live_google_stack`] returns `None` and the commit
//! registry leaves `/mail` / `/drive` / `/ga` **unregistered** — a commit then fails with a clear
//! "no driver / not configured" cause rather than faking success. The `client_secret` and the
//! refresh token are `qfs_secrets::Secret` (envelope-encrypted at rest, redacting `Debug`), never
//! logged and never placed on argv.
//!
//! ## The account model
//! One consent serves all three Google drivers: the refresh token is stored ONCE under
//! `google:<email>:refresh_token` (`qfs_google_auth::refresh_token_key`), and a single
//! [`StoredTokenSource`] + [`GoogleApiClient`] (built per account email) is shared by the gmail,
//! drive, and analytics clients. The active account email is resolved from
//! [`GOOGLE_ACCOUNT_ENV`] (the explicit agent/CI override) else the active `google` connection
//! selection (`qfs connection use google <email>`).
//!
//! ## The live consent flow is a documented SEAM
//! [`run_google_consent`] wires the real `qfs_google_auth::authorize` loopback browser flow (build
//! auth URL → open consent → capture the redirect code → exchange → persist the refresh token). The
//! browser open + the human approval are interactive and **not hermetically testable**, so this is
//! plumbing wired but left a documented seam — it is reached only from the opt-in
//! `QFS_GOOGLE_CONSENT` path in [`crate::connection`], never from a tested code path.

use std::sync::Arc;
use std::time::Duration;

use qfs_google_auth::{AuthError, GoogleApiClient, OAuthClient, StoredTokenSource, TokenSource};
use qfs_secrets::{EnvStore, Secret, Secrets};

/// Env var carrying the operator's Google **Desktop** OAuth client id (non-secret). Absent ⇒ the
/// Google drivers are not registered (fail closed).
pub const GOOGLE_CLIENT_ID_ENV: &str = "QFS_GOOGLE_CLIENT_ID";
/// Env var carrying the operator's Google Desktop OAuth **client secret**. Read into a
/// [`Secret`] (redacting); absent ⇒ the Google drivers are not registered (fail closed).
pub const GOOGLE_CLIENT_SECRET_ENV: &str = "QFS_GOOGLE_CLIENT_SECRET";
/// Env var naming the active Google **account email** (the explicit agent/CI override for the
/// account whose `google:<email>:refresh_token` the token source uses). Falls back to the active
/// `google` connection selection.
pub const GOOGLE_ACCOUNT_ENV: &str = "QFS_GOOGLE_ACCOUNT";
/// Opt-in flag (any value) that switches `qfs connection add gmail|gdrive|ga <name>` from the
/// out-of-band stdin refresh-token path to the interactive loopback browser consent flow
/// ([`run_google_consent`] — the documented seam).
pub const GOOGLE_CONSENT_ENV: &str = "QFS_GOOGLE_CONSENT";

/// How long the loopback consent listener waits for the redirect before giving up. A human who
/// never approves yields a timeout rather than hanging forever.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);

/// The composed, account-bound Google API client the three driver clients share. Built once per
/// commit from the resolved app config + account email; cloned (`Arc`) into the gmail/drive/ga
/// clients so one transport + one token cache serves the whole Google stack.
pub struct GoogleStack {
    /// The authenticated client (bearer injection + refresh-on-401) the gmail/drive/ga clients wrap.
    pub api: Arc<GoogleApiClient>,
}

/// Read the operator's Google OAuth app credentials from the environment. `None` (fail closed) when
/// either the client id or the client secret is absent/empty. The pure decision is factored into
/// [`config_from`] so the fail-closed logic is unit-tested without touching the process environment.
fn google_app_config() -> Option<(String, Secret)> {
    config_from(
        std::env::var(GOOGLE_CLIENT_ID_ENV).ok(),
        std::env::var(GOOGLE_CLIENT_SECRET_ENV).ok(),
    )
}

/// Pure fail-closed gate: both the client id AND the client secret must be present and non-empty,
/// else `None`. The secret is wrapped in a redacting [`Secret`] immediately (never a plain field).
fn config_from(
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Option<(String, Secret)> {
    let client_id = non_empty(client_id)?;
    let client_secret = non_empty(client_secret)?;
    Some((client_id, Secret::from(client_secret)))
}

/// `Some(trimmed)` only when the value is present and not empty after trimming; `None` otherwise.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The union of the three Google drivers' least-privilege scopes — what the shared consent requests
/// so one authorization serves gmail + drive + analytics (least privilege per RFD §10: the modify +
/// compose Gmail scopes, the Drive scope, and the read-only Analytics scope — never a broader grant).
/// Used only for the consent auth-URL; token refresh is scope-agnostic.
fn all_google_scopes() -> Vec<String> {
    vec![
        qfs_driver_gmail::GMAIL_MODIFY_SCOPE.to_string(),
        qfs_driver_gmail::GMAIL_COMPOSE_SCOPE.to_string(),
        qfs_driver_gdrive::DRIVE_SCOPE.to_string(),
        qfs_driver_ga::ANALYTICS_READONLY_SCOPE.to_string(),
    ]
}

/// The per-driver least-privilege scope set a single Google driver's consent requests. Used by the
/// [`run_google_consent`] seam so `connection add gmail` asks for only the Gmail scopes, etc. An
/// unknown driver yields an empty set (it requests nothing).
fn consent_scopes(driver: &str) -> Vec<String> {
    match driver {
        "gmail" => vec![
            qfs_driver_gmail::GMAIL_MODIFY_SCOPE.to_string(),
            qfs_driver_gmail::GMAIL_COMPOSE_SCOPE.to_string(),
        ],
        "gdrive" => vec![qfs_driver_gdrive::DRIVE_SCOPE.to_string()],
        "ga" => vec![qfs_driver_ga::ANALYTICS_READONLY_SCOPE.to_string()],
        _ => Vec::new(),
    }
}

/// Resolve the active Google **account email**: the explicit [`GOOGLE_ACCOUNT_ENV`] override first
/// (the agent/CI path), else the active `google` connection selection. `None` (fail closed) when no
/// account is selected — without an account email there is no refresh token to mint from, so the
/// drivers are left unregistered rather than bound to nothing.
fn resolve_account_email() -> Option<String> {
    if let Some(email) = non_empty(std::env::var(GOOGLE_ACCOUNT_ENV).ok()) {
        return Some(email);
    }
    crate::connection::active_connection("google").filter(|s| !s.is_empty())
}

/// Resolve the credential store the commit path reads (mirrors `commit::networked_credential`): the
/// envelope-encrypted SQLite store when `QFS_PASSPHRASE` + the Project DB are available, else the
/// process-env store (the agent/CI path). The refresh token is read LAZILY by the token source at
/// request time, so a missing/locked store surfaces as a clear per-leg auth error, never a panic.
fn commit_secret_store() -> Arc<dyn Secrets> {
    match crate::connection::open_store_for_commit() {
        Some(sqlite) => Arc::new(sqlite),
        None => Arc::new(EnvStore::from_process_env()),
    }
}

/// The current UTC time as Unix-epoch **nanoseconds** — the clock anchor the OAuth token exchange /
/// refresh stamp expiry against. A pre-epoch clock (impossible in practice) reads as 0 rather than
/// panicking.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Build the account-bound [`GoogleStack`] for the live commit registry, or `None` (fail closed)
/// when the operator's OAuth app credentials or the active account email are absent.
///
/// Composition: the shared reqwest transport (the binary's [`crate::transport::google_transport`])
/// feeds BOTH the [`OAuthClient`] (token exchange/refresh) and the [`GoogleApiClient`] (API calls).
/// A per-account [`StoredTokenSource`] reads `google:<email>:refresh_token` from the resolved
/// credential store and refreshes transparently; the authenticated client injects the bearer and
/// retries once on a 401. The returned `api` is shared (`Arc`) by the gmail/drive/ga driver clients.
///
/// The credential is read **lazily** (at request-build time), so a missing refresh token does not
/// fail registry build — it surfaces as a clear per-leg auth error at commit time (honest).
#[must_use]
pub fn live_google_stack() -> Option<GoogleStack> {
    let (client_id, client_secret) = google_app_config()?;
    let email = resolve_account_email()?;
    let transport = crate::transport::google_transport();
    let store = commit_secret_store();
    let oauth = OAuthClient::new(
        client_id,
        client_secret,
        all_google_scopes(),
        transport.clone(),
    );
    let tokens: Arc<dyn TokenSource> = Arc::new(StoredTokenSource::new(email, store, oauth));
    let api = Arc::new(GoogleApiClient::new(transport, tokens));
    Some(GoogleStack { api })
}

/// **Documented SEAM — the live loopback browser consent flow.** Run `qfs_google_auth::authorize`
/// for `driver`: build the OAuth client over the real transport + the supplied store, advertise the
/// `http://localhost:<port>` loopback redirect, open the consent URL, capture the redirect code,
/// exchange it for tokens, and persist the refresh token under `google:<email>:refresh_token`
/// (shared across gmail/gdrive/ga). Returns the authorized account email.
///
/// The browser open + the human approval are interactive and **not hermetically testable**, so this
/// is plumbing wired but never exercised by a test: it is reached only from the opt-in
/// `QFS_GOOGLE_CONSENT` branch in [`crate::connection`]. The default `connection add` path still
/// provisions a refresh token out of band (from stdin), so green never depends on this round-trip.
///
/// # Errors
/// [`AuthError`] if the OAuth app credentials are absent, or for any step of the flow (bind, build
/// URL, denied/timeout, token exchange, profile lookup, store).
pub fn run_google_consent(driver: &str, store: Arc<dyn Secrets>) -> Result<String, AuthError> {
    let (client_id, client_secret) = google_app_config().ok_or_else(|| AuthError::Invalid {
        reason: format!(
            "{GOOGLE_CLIENT_ID_ENV} / {GOOGLE_CLIENT_SECRET_ENV} must be set to a registered \
             Google Desktop OAuth app before running consent"
        ),
    })?;
    let oauth = OAuthClient::new(
        client_id,
        client_secret,
        consent_scopes(driver),
        crate::transport::google_transport(),
    );
    // The CLI prints the consent URL; the human opens it and approves. (A headless caller could
    // inject an opener that drives the redirect — the test seam — but the live flow is interactive.)
    let opener: Box<qfs_google_auth::ConsentOpener> = Box::new(|url: &str| {
        println!("Open this URL to authorize qfs, then return to the terminal:\n{url}");
        Ok(())
    });
    let account =
        qfs_google_auth::authorize(&oauth, &store, &*opener, now_nanos(), CONSENT_TIMEOUT)?;
    Ok(account.email)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fail-closed: the OAuth app config requires BOTH the client id and the client secret, each
    /// non-empty. Any missing/blank half yields `None`, so the Google drivers are never registered
    /// without a fully configured operator app. Pure (no env, no stores) so it is hermetic.
    #[test]
    fn config_is_fail_closed_without_both_credentials() {
        assert!(config_from(None, None).is_none(), "no creds ⇒ None");
        assert!(
            config_from(Some("id".into()), None).is_none(),
            "missing secret ⇒ None"
        );
        assert!(
            config_from(None, Some("secret".into())).is_none(),
            "missing id ⇒ None"
        );
        assert!(
            config_from(Some("  ".into()), Some("secret".into())).is_none(),
            "blank id ⇒ None"
        );
        assert!(
            config_from(Some("id".into()), Some("".into())).is_none(),
            "empty secret ⇒ None"
        );
        let ok = config_from(Some("id".into()), Some("secret".into()));
        assert!(ok.is_some(), "both present ⇒ Some");
        // The secret half is a redacting Secret — it must never surface its value on Debug.
        let (id, secret) = ok.unwrap();
        assert_eq!(id, "id");
        assert!(
            !format!("{secret:?}").contains("secret"),
            "the client secret must be redacted on Debug, never printed"
        );
    }

    /// The shared consent scope union is exactly the four least-privilege Google scopes (modify +
    /// compose Gmail, Drive, read-only Analytics) — no broader grant leaks in.
    #[test]
    fn scope_union_is_least_privilege() {
        let scopes = all_google_scopes();
        assert!(scopes.contains(&qfs_driver_gmail::GMAIL_MODIFY_SCOPE.to_string()));
        assert!(scopes.contains(&qfs_driver_gmail::GMAIL_COMPOSE_SCOPE.to_string()));
        assert!(scopes.contains(&qfs_driver_gdrive::DRIVE_SCOPE.to_string()));
        assert!(scopes.contains(&qfs_driver_ga::ANALYTICS_READONLY_SCOPE.to_string()));
        // No `https://mail.google.com/` full scope and no Drive-delete-only broadening.
        assert!(
            !scopes.iter().any(|s| s == "https://mail.google.com/"),
            "the broad full-mailbox scope must never be requested"
        );
    }

    /// Per-driver consent requests only that driver's scopes (least privilege); an unknown driver
    /// requests nothing.
    #[test]
    fn per_driver_consent_scopes_are_narrow() {
        assert_eq!(consent_scopes("gmail").len(), 2);
        assert_eq!(consent_scopes("gdrive").len(), 1);
        assert_eq!(consent_scopes("ga").len(), 1);
        assert!(consent_scopes("github").is_empty());
    }
}
