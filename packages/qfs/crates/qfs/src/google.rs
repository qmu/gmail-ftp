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
//! supplies its id/secret either by saving the `credentials.json` into qfs's own encrypted DB
//! (`cat credentials.json | qfs connection add google-app default` — read first by
//! [`app_config_from_store`], so qfs owns the app and does not depend on an external file) or via
//! [`GOOGLE_CLIENT_ID_ENV`] / [`GOOGLE_CLIENT_SECRET_ENV`] (the agent/CI fallback). Absent either
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

/// The store driver/connection the Google OAuth **app** credentials are saved under, so a qfs user
/// can offer them once and qfs owns them in its own encrypted DB (rather than depending on an env
/// var or an external `credentials.json` that "isn't always there"). Set with:
/// `cat credentials.json | qfs connection add google-app default` — the stored value is the Google
/// `credentials.json` (or a `{"client_id":…,"client_secret":…}` blob); read back by
/// [`app_config_from_store`].
pub const GOOGLE_APP_DRIVER: &str = "google-app";
/// The connection name the app credentials are stored under (a single app per install).
pub const GOOGLE_APP_CONNECTION: &str = "default";

/// Read the operator's Google OAuth app credentials: **qfs's own encrypted store first** (the
/// user-offered `google-app/default` credentials), then the [`GOOGLE_CLIENT_ID_ENV`] /
/// [`GOOGLE_CLIENT_SECRET_ENV`] environment (the agent/CI + back-compat path). `None` (fail closed)
/// when neither yields both an id and a secret.
fn google_app_config() -> Option<(String, Secret)> {
    app_config_from_store().or_else(|| {
        config_from(
            std::env::var(GOOGLE_CLIENT_ID_ENV).ok(),
            std::env::var(GOOGLE_CLIENT_SECRET_ENV).ok(),
        )
    })
}

/// Read the OAuth app credentials saved in qfs's encrypted store under `google-app/default`. `None`
/// when the store is unavailable/locked (no `QFS_PASSPHRASE`), the key is absent, or the stored blob
/// does not parse — the env path is tried next, so a missing store is never fatal here.
fn app_config_from_store() -> Option<(String, Secret)> {
    use qfs_secrets::{ConnectionId, CredentialKey, DriverId};
    let store = crate::connection::open_store_for_commit()?;
    let key = CredentialKey::new(
        DriverId(GOOGLE_APP_DRIVER.to_string()),
        ConnectionId::new(GOOGLE_APP_CONNECTION).ok()?,
    );
    let blob = store.get(&key).ok()?;
    let (id, secret) = parse_app_credentials(blob.expose_str()?)?;
    config_from(Some(id), Some(secret))
}

/// Parse a stored app-credentials blob into `(client_id, client_secret)`. Accepts Google's downloaded
/// `credentials.json` shape (`{"installed":{…}}` or `{"web":{…}}`) **and** a flat
/// `{"client_id":…,"client_secret":…}`. Pure (no I/O), so it is unit-tested directly.
fn parse_app_credentials(blob: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    // Unwrap Google's `installed`/`web` envelope when present.
    let inner = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
    let id = inner.get("client_id")?.as_str()?.trim().to_string();
    let secret = inner.get("client_secret")?.as_str()?.trim().to_string();
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
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

/// Build a [`GoogleStack`] bound to **one given account email**, or `None` (fail closed) when the
/// operator's OAuth app credentials are absent. The mount-bound account model (ADR 0008) builds one
/// stack per connect-created mount, so N accounts of one driver coexist as N stacks in one process.
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
pub fn google_stack_for_account(email: &str) -> Option<GoogleStack> {
    let (client_id, client_secret) = google_app_config()?;
    let transport = crate::transport::google_transport();
    let store = commit_secret_store();
    let oauth = OAuthClient::new(
        client_id,
        client_secret,
        all_google_scopes(),
        transport.clone(),
    );
    let tokens: Arc<dyn TokenSource> =
        Arc::new(StoredTokenSource::new(email.to_string(), store, oauth));
    let api = Arc::new(GoogleApiClient::new(transport, tokens));
    Some(GoogleStack { api })
}

/// Build the [`GoogleStack`] for the **selected** account (the legacy single-active-account path):
/// resolve the account email ([`GOOGLE_ACCOUNT_ENV`] else the active `google` connection) and bind
/// a stack to it via [`google_stack_for_account`]. `None` (fail closed) when no account is selected
/// or the OAuth app credentials are absent.
#[must_use]
pub fn live_google_stack() -> Option<GoogleStack> {
    google_stack_for_account(&resolve_account_email()?)
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
pub fn run_google_consent(store: Arc<dyn Secrets>) -> Result<String, AuthError> {
    let (client_id, client_secret) = google_app_config().ok_or_else(|| AuthError::Invalid {
        reason: format!(
            "{GOOGLE_CLIENT_ID_ENV} / {GOOGLE_CLIENT_SECRET_ENV} must be set to a registered \
             Google Desktop OAuth app before running consent"
        ),
    })?;
    let oauth = OAuthClient::new(
        client_id,
        client_secret,
        all_google_scopes(),
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

    #[test]
    fn parses_googles_installed_credentials_json() {
        // The exact shape Google Cloud Console hands you for a Desktop OAuth client.
        let blob = r#"{"installed":{"client_id":"abc.apps.googleusercontent.com",
            "client_secret":"s3cr3t","redirect_uris":["http://localhost"]}}"#;
        assert_eq!(
            parse_app_credentials(blob),
            Some((
                "abc.apps.googleusercontent.com".to_string(),
                "s3cr3t".to_string()
            ))
        );
    }

    #[test]
    fn parses_a_flat_client_id_secret_blob_and_rejects_partial() {
        assert_eq!(
            parse_app_credentials(r#"{"client_id":"id","client_secret":"sec"}"#),
            Some(("id".to_string(), "sec".to_string()))
        );
        assert!(parse_app_credentials(r#"{"client_id":"id"}"#).is_none());
        assert!(parse_app_credentials(r#"{"client_id":"","client_secret":"x"}"#).is_none());
        assert!(parse_app_credentials("not json").is_none());
    }

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
}
