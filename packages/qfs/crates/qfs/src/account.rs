//! ADR 0008 §3 — the **`qfs app` / `qfs account` composition root** (EPIC 20260702120000 /
//! ticket 20260702120040): the per-layer verbs that dissolve the `connection` grab-bag.
//!
//! - **`qfs app`** owns OAuth **app registrations** — the operator's client id/secret (today:
//!   Google's `credentials.json`), sealed in the vault under the `<provider>-app` driver exactly
//!   where the retired connection namespace used to put it (`crate::google::google_app_config`
//!   keeps reading it unchanged).
//! - **`qfs account`** owns external **service accounts** — the token + the recorded consent.
//!   For Google, ONE account-level authorization serves gmail + gdrive + ga (the shared
//!   `google:<email>:refresh_token`, the scope union, and the ADR-0008 incremental-auth fix): a
//!   terminal runs the live browser consent (the old `QFS_GOOGLE_CONSENT=1` opt-in is retired —
//!   `qfs account add google` on a TTY *is* the opt-in), automation pipes a refresh token on
//!   stdin with the email as the label. Other cloud providers (github/slack/objstore/cf) pipe or
//!   prompt their token per label.
//!
//! ## Consent keying (ADR 0008 §4 — mount-bound)
//! Consent is recorded per Google DRIVER keyed by the ACCOUNT EMAIL (per `(provider, label)` for
//! the other clouds) — exactly the `(kind, account)` pair the commit-time bind gate consults for
//! a connect-created mount. There is no selection state: an authorized account becomes usable by
//! connecting a mount to it (`qfs connect /mail --driver gmail --account <email>`).
//!
//! ## Secret hygiene (RFD §10)
//! Tokens arrive on stdin or an echo-off TTY prompt, never argv; they are sealed by the vault and
//! never printed back. `app list` / `account list` render selectors + metadata only.

use std::io::Read;
use std::sync::Arc;

use qfs_cmd::AccountAction;
use qfs_secrets::{is_cloud_driver, ConnectionId, CredentialKey, DriverId, Secret, Secrets};
use rusqlite::Connection;

use crate::connection::{open_project_conn, open_store, require_signed_in};
use crate::secret_store;

/// The Google provider's three drivers — one account authorization serves them all (the shared
/// refresh token; ADR 0008 §4 "one consent, many drivers").
const GOOGLE_DRIVERS: [&str; 3] = ["gmail", "gdrive", "ga"];

/// The injected app/account launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_account(action: &AccountAction) -> i32 {
    match run_inner(action) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("qfs: error: {e}");
            1
        }
    }
}

fn run_inner(action: &AccountAction) -> Result<String, String> {
    match action {
        AccountAction::AppAdd { provider } => app_add(provider),
        AccountAction::AppList => app_list(),
        AccountAction::AppRemove { provider } => app_remove(provider),
        AccountAction::Add { provider, label } => match provider.as_str() {
            "google" => add_google(label.as_deref()),
            other if is_cloud_driver(&DriverId(other.to_string())) => {
                add_cloud(other, label.as_deref().unwrap_or("default"))
            }
            other => Err(format!(
                "`{other}` is not a cloud provider — accounts exist for google, github, slack, \
                 objstore, cf. A local source (SQL file, git repo) needs no account: declare it \
                 with CREATE CONNECTION / `qfs connect`"
            )),
        },
        AccountAction::List => list_accounts(),
        AccountAction::Remove { provider, label } => match provider.as_str() {
            "google" => remove_google(label),
            other => remove_cloud(other, label),
        },
        AccountAction::Rotate { provider, label } => rotate_account(provider, label),
        AccountAction::Revoke { provider, label } => revoke_account(provider, label),
    }
}

/// `qfs account rotate <provider> <label>` — re-mint the account's secret (t79, moved here from
/// the retired `connection` namespace): read a NEW secret from stdin, re-seal it, and clear any
/// revocation. The offboarding answer — replace, not un-grant.
fn rotate_account(provider: &str, label: &str) -> Result<String, String> {
    // A cloud account carries the same sign-in gate as `add` (a cloud credential is unusable for
    // an unauthenticated operator); resolve identity BEFORE touching stdin.
    if is_cloud_driver(&DriverId(provider.to_string())) {
        let _ = require_signed_in(provider)?;
    }
    let value = read_secret(
        "new secret",
        &format!("printf %s \"$TOKEN\" | qfs account rotate {provider} {label}"),
    )?;
    let conn_id = ConnectionId::new(label).map_err(|e| e.to_string())?;
    let key = CredentialKey::new(DriverId(provider.to_string()), conn_id);
    let store = open_store()?;
    store
        .rotate(&key, Secret::from(value))
        .map_err(|e| format!("rotating the credential: {e}"))?;
    crate::connection::emit_connection_audit("ROTATE", &format!("{provider}/{label}"));
    Ok(format!(
        "rotated {provider}/{label} (secret re-minted; any revocation cleared)"
    ))
}

/// `qfs account revoke <provider> <label>` — mark the account's credential unresolvable (t79,
/// moved here from the retired `connection` namespace): a later bind fails closed (the secret is
/// never returned); other accounts keep working. Re-minting (`qfs account rotate`) restores use.
fn revoke_account(provider: &str, label: &str) -> Result<String, String> {
    let conn_id = ConnectionId::new(label).map_err(|e| e.to_string())?;
    let key = CredentialKey::new(DriverId(provider.to_string()), conn_id);
    let store = open_store()?;
    store
        .revoke(&key)
        .map_err(|e| format!("revoking the account: {e}"))?;
    crate::connection::emit_connection_audit("REVOKE", &format!("{provider}/{label}"));
    Ok(format!(
        "revoked {provider}/{label} (it can no longer resolve until re-minted with `qfs account rotate`)"
    ))
}

/// The `<provider>-app` driver id an app registration is sealed under (the same key the retired
/// connection namespace wrote, so `google_app_config` reads on unchanged).
fn app_key(provider: &str) -> Result<CredentialKey, String> {
    if provider != "google" {
        return Err(format!(
            "no app registration exists for `{provider}` — today the OAuth-app layer serves \
             google (its Desktop-app credentials.json); other providers authenticate per account \
             token"
        ));
    }
    let conn = ConnectionId::new("default").map_err(|e| e.to_string())?;
    Ok(CredentialKey::new(
        DriverId(format!("{provider}-app")),
        conn,
    ))
}

/// `qfs app add google < credentials.json` — seal the operator's OAuth app credentials.
fn app_add(provider: &str) -> Result<String, String> {
    let key = app_key(provider)?;
    let value = read_secret(
        "app credentials",
        "cat credentials.json | qfs app add google",
    )?;
    let store = open_store()?;
    store
        .put(&key, Secret::from(value))
        .map_err(|e| format!("storing the app credentials: {e}"))?;
    Ok(format!(
        "registered the {provider} OAuth app (credentials sealed in the vault; `qfs account add \
         {provider}` can now authorize accounts)"
    ))
}

/// `qfs app list` — the registered OAuth apps (provider + created_at; never a secret).
fn app_list() -> Result<String, String> {
    let store = open_store()?;
    let records = store
        .list(None)
        .map_err(|e| format!("listing app registrations: {e}"))?;
    let apps: Vec<String> = records
        .iter()
        .filter(|r| r.driver.as_str().ends_with("-app"))
        .map(|r| {
            let provider = r.driver.as_str().trim_end_matches("-app");
            format!("{provider}\tregistered {}", r.created_at)
        })
        .collect();
    if apps.is_empty() {
        return Ok(
            "no OAuth apps registered — `cat credentials.json | qfs app add google`".to_string(),
        );
    }
    Ok(apps.join("\n"))
}

/// `qfs app remove <provider>` — delete the app registration (accounts' tokens stay).
fn app_remove(provider: &str) -> Result<String, String> {
    let key = app_key(provider)?;
    let store = open_store()?;
    store
        .remove(&key)
        .map_err(|e| format!("removing the app registration: {e}"))?;
    Ok(format!("removed the {provider} OAuth app registration"))
}

/// `qfs account add google [email]` — authorize a Google account. On a terminal with no piped
/// token this runs the LIVE loopback browser consent (the documented non-hermetic seam — the old
/// `QFS_GOOGLE_CONSENT` env opt-in is retired; invoking this verb on a TTY is the opt-in);
/// automation pipes the refresh token with the email as the label.
fn add_google(label: Option<&str>) -> Result<String, String> {
    let subject = require_signed_in("gmail")?;
    let store = open_store()?;

    let email = if crate::tty::stdin_is_terminal() {
        // Interactive: the real browser consent (requests the PROVIDER scope union — one
        // authorization serves gmail+gdrive+ga; persists the refresh token + selects the account).
        let store_arc: Arc<dyn Secrets> = Arc::new(store);
        crate::google::run_google_consent(store_arc)
            .map_err(|e| format!("google consent failed: {e}"))?
    } else {
        // Automation: the refresh token on stdin, the email as the label.
        let Some(email) = label else {
            return Err(
                "the token-import path needs the account email — `printf %s \
                        \"$REFRESH_TOKEN\" | qfs account add google you@example.com`"
                    .into(),
            );
        };
        let token = read_secret(
            "refresh token",
            "printf %s \"$REFRESH_TOKEN\" | qfs account add google you@example.com",
        )?;
        let key = qfs_google_auth::refresh_token_key(email).map_err(|e| e.to_string())?;
        store
            .put(&key, Secret::from(token))
            .map_err(|e| format!("storing the refresh token: {e}"))?;
        email.to_string()
    };

    // Record the account-level consent per Google DRIVER, keyed by the ACCOUNT EMAIL (ADR 0008
    // §4 — the mount carries the account, so the commit-time bind gate consults the mount's
    // `(driver, account)`). No selection is made: the account becomes usable by connecting a
    // mount to it (`qfs connect /mail --driver gmail --account <email>`).
    let proj = open_project_conn()?;
    record_google_consents(&proj, &subject, &email)?;
    Ok(format!(
        "authorized google account {email} (one authorization serves mail, drive, and analytics; \
         consent granted by {subject}) — mount it with `qfs connect /mail --driver gmail --account {email}`"
    ))
}

/// Consent rows for the three Google drivers, keyed by the account email — what the mount-bound
/// bind gate consults for a `(kind, account)` cloud mount (see the module doc).
fn record_google_consents(proj: &Connection, subject: &str, email: &str) -> Result<(), String> {
    for driver in GOOGLE_DRIVERS {
        secret_store::db_record_consent(proj, driver, email, subject, google_scope(driver))
            .map_err(|e| format!("recording consent for {driver}: {e}"))?;
    }
    Ok(())
}

/// The §10 consent-scope hint recorded per Google driver (metadata; the live token negotiation is
/// the OAuth client's).
fn google_scope(driver: &str) -> &'static str {
    match driver {
        "gmail" => "gmail.modify gmail.compose",
        "gdrive" => "drive",
        _ => "analytics.readonly",
    }
}

/// `qfs account add <provider> [label]` for the non-Google cloud drivers: the token on stdin (or
/// an echo-off TTY prompt), sealed under `(provider, label)`, with the consent recorded.
fn add_cloud(provider: &str, label: &str) -> Result<String, String> {
    let subject = require_signed_in(provider)?;
    let token = if crate::tty::stdin_is_terminal() {
        crate::tty::prompt_secret(&format!("{provider} token (input hidden): "))?
            .expose_str()
            .ok_or("the token is not valid UTF-8")?
            .to_string()
    } else {
        read_secret(
            "token",
            &format!("printf %s \"$TOKEN\" | qfs account add {provider} {label}"),
        )?
    };
    let conn_id = ConnectionId::new(label).map_err(|e| e.to_string())?;
    let key = CredentialKey::new(DriverId(provider.to_string()), conn_id);
    let store = open_store()?;
    store
        .put(&key, Secret::from(token))
        .map_err(|e| format!("storing the token: {e}"))?;
    let proj = open_project_conn()?;
    secret_store::db_record_consent(&proj, provider, label, &subject, "")
        .map_err(|e| format!("recording consent: {e}"))?;
    Ok(format!(
        "authorized {provider} account `{label}` (consent granted by {subject})"
    ))
}

/// `qfs account list` — the authorized service accounts (provider + label + created_at; never a
/// token). Google accounts render their decoded email.
fn list_accounts() -> Result<String, String> {
    let store = open_store()?;
    let records = store
        .list(None)
        .map_err(|e| format!("listing accounts: {e}"))?;
    let accounts: Vec<String> = records
        .iter()
        .filter_map(|r| {
            let driver = r.driver.as_str();
            if driver == "google" {
                let email = qfs_google_auth::decode_account_email(r.connection.as_str());
                Some(format!("google\t{email}\tauthorized {}", r.created_at))
            } else if is_cloud_driver(&r.driver) {
                Some(format!(
                    "{driver}\t{}\tauthorized {}",
                    r.connection.as_str(),
                    r.created_at
                ))
            } else {
                None
            }
        })
        .collect();
    if accounts.is_empty() {
        return Ok(
            "no service accounts yet — `qfs account add google` (or github/slack/…)".to_string(),
        );
    }
    Ok(accounts.join("\n"))
}

/// `qfs account remove google <email>` — delete the refresh token and the three drivers' consent
/// rows (data-sovereignty: deletion is first-class and complete). Mounts bound to the account
/// stay defined and fail closed until reconnected to another account.
fn remove_google(email: &str) -> Result<String, String> {
    let key = qfs_google_auth::refresh_token_key(email).map_err(|e| e.to_string())?;
    let store = open_store()?;
    store
        .remove(&key)
        .map_err(|e| format!("removing the refresh token: {e}"))?;
    let proj = open_project_conn()?;
    for driver in GOOGLE_DRIVERS {
        delete_consent(&proj, driver, email)?;
    }
    Ok(format!(
        "removed google account {email} (token and consents deleted)"
    ))
}

/// `qfs account remove <provider> <label>` — delete the token + the consent row.
fn remove_cloud(provider: &str, label: &str) -> Result<String, String> {
    let conn_id = ConnectionId::new(label).map_err(|e| e.to_string())?;
    let key = CredentialKey::new(DriverId(provider.to_string()), conn_id);
    let store = open_store()?;
    store
        .remove(&key)
        .map_err(|e| format!("removing the token: {e}"))?;
    let proj = open_project_conn()?;
    delete_consent(&proj, provider, label)?;
    Ok(format!(
        "removed {provider} account `{label}` (token and consent deleted)"
    ))
}

/// Delete one consent row (the t54 ledger keeps history via the audit chain; the LIVE row gates
/// binds, so a removed account must not keep gating open).
fn delete_consent(proj: &Connection, driver: &str, connection: &str) -> Result<(), String> {
    proj.execute(
        "DELETE FROM connection_consent WHERE driver = ?1 AND connection = ?2",
        rusqlite::params![driver, connection],
    )
    .map_err(|e| format!("deleting consent for {driver}: {e}"))?;
    Ok(())
}

/// Read a single secret value from stdin, never argv (mirrors `connection.rs`'s reader).
fn read_secret(what: &str, example: &str) -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("reading the {what} from stdin: {e}"))?;
    let value = buf.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        return Err(format!("no {what} on stdin — pipe it, e.g. `{example}`"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENV_LOCK;

    fn with_fresh_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_pass = std::env::var_os("QFS_PASSPHRASE");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::set_var("QFS_PASSPHRASE", "account-test-pass");
        let out = f();
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_pass {
            Some(v) => std::env::set_var("QFS_PASSPHRASE", v),
            None => std::env::remove_var("QFS_PASSPHRASE"),
        }
        out
    }

    /// The Google token-import bookkeeping: the refresh token lands under the account key and all
    /// three Google drivers get a consent row keyed by the ACCOUNT EMAIL (ADR 0008 — what the
    /// mount-bound bind gate consults). No selection state exists (migration #11 dropped it).
    /// Removal deletes all of it (deletion is complete).
    #[test]
    fn google_account_bookkeeping_round_trips() {
        with_fresh_home(|| {
            // Seed the pieces `add_google`'s non-stdin internals write (the stdin read itself is
            // exercised by the release smoke, not in-process).
            let store = open_store().unwrap();
            let key = qfs_google_auth::refresh_token_key("you@example.com").unwrap();
            store.put(&key, Secret::from("1//refresh")).unwrap();
            let proj = open_project_conn().unwrap();
            record_google_consents(&proj, "op@example.com", "you@example.com").unwrap();

            for driver in GOOGLE_DRIVERS {
                assert!(
                    secret_store::db_get_consent(&proj, driver, "you@example.com").is_some(),
                    "{driver} consent recorded under the account email"
                );
            }
            drop(proj);

            let out = remove_google("you@example.com").unwrap();
            assert!(out.contains("you@example.com"));
            let proj = open_project_conn().unwrap();
            for driver in GOOGLE_DRIVERS {
                assert!(
                    secret_store::db_get_consent(&proj, driver, "you@example.com").is_none(),
                    "{driver} consent deleted"
                );
            }
        });
    }

    /// An unknown provider is an actionable error naming the cloud set; an app registration for a
    /// non-google provider is refused (only google has an OAuth-app layer today).
    #[test]
    fn unknown_providers_are_actionable_errors() {
        with_fresh_home(|| {
            let err = run_inner(&AccountAction::Add {
                provider: "sqlite".into(),
                label: None,
            })
            .unwrap_err();
            assert!(err.contains("not a cloud provider"), "{err}");
            assert!(err.contains("CREATE CONNECTION"), "actionable: {err}");
            let err = app_key("github").unwrap_err();
            assert!(err.contains("github"), "{err}");
        });
    }

    /// `app add` → `app list` → `app remove` round-trips the google-app registration under the
    /// SAME key `google_app_config` reads (the retired connection-add path's key).
    #[test]
    fn app_registration_round_trips_under_the_legacy_key() {
        with_fresh_home(|| {
            let store = open_store().unwrap();
            let key = app_key("google").unwrap();
            assert_eq!(key.driver.as_str(), "google-app");
            assert_eq!(key.connection.as_str(), "default");
            store.put(&key, Secret::from("{\"installed\":{}}")).unwrap();
            drop(store);

            let listed = app_list().unwrap();
            assert!(listed.contains("google"), "{listed}");
            let removed = app_remove("google").unwrap();
            assert!(removed.contains("google"));
            let listed = app_list().unwrap();
            assert!(listed.contains("no OAuth apps"), "{listed}");
        });
    }
}
