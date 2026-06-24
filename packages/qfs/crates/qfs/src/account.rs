//! The `qfs account` composition root: the real credential-store I/O that backs
//! `qfs account add/list/use/remove`, injected into `qfs-cmd` as the [`qfs_cmd::AccountLauncher`].
//!
//! `qfs-cmd` may not depend on the concrete `qfs-secrets` backend (the dep_direction guard), so —
//! exactly like the shell / serve / describe launchers — the binary owns this and `qfs-cmd` only
//! parses the verb and calls in.
//!
//! ## Security (RFD §10)
//! - The credential **value** is read from **stdin**, never from argv (argv leaks into shell
//!   history and `ps`).
//! - Credentials live in the encrypted [`qfs_secrets::LocalStore`] (`0600`, AEAD, argon2id KDF).
//!   The KDF passphrase comes from the `QFS_PASSPHRASE` env var (the no-keyring path); the per-store
//!   salt is created once beside the vault. Secrets are never printed, logged, or echoed.

use std::io::Read;
use std::path::PathBuf;

use qfs_cmd::AccountAction;
use qfs_secrets::{
    default_credentials_path, load_or_create_salt, AccountId, CredentialKey, DriverId, LocalStore,
    Secret, Secrets,
};

/// The injected account launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_account(action: &AccountAction) -> i32 {
    match run_inner(action) {
        Ok(msg) => {
            eprintln!("qfs: {msg}");
            0
        }
        Err(e) => {
            eprintln!("qfs: error: {e}");
            1
        }
    }
}

/// Open the encrypted credential store: resolve the path, load-or-create the salt, and unlock with
/// `QFS_PASSPHRASE`. Returns the store and its on-disk path.
fn open_store() -> Result<(LocalStore, PathBuf), String> {
    let cred = default_credentials_path()
        .ok_or("cannot determine the credentials path (set HOME or XDG_CONFIG_HOME)")?;
    let salt =
        load_or_create_salt(&salt_path(&cred)).map_err(|e| format!("credential salt: {e}"))?;
    let pass = std::env::var("QFS_PASSPHRASE").map_err(|_| {
        "QFS_PASSPHRASE is not set — export it to unlock the encrypted credential store".to_string()
    })?;
    if pass.is_empty() {
        return Err("QFS_PASSPHRASE is empty".into());
    }
    let store = LocalStore::from_passphrase(&cred, &Secret::from(pass), &salt)
        .map_err(|e| format!("opening the credential store: {e}"))?;
    Ok((store, cred))
}

/// Open the credential store for the **commit resolver** (read path): the same encrypted
/// [`LocalStore`] `account add` writes to, when `QFS_PASSPHRASE` + the vault both exist. Returns
/// `None` (best-effort, never an error) when the store cannot be unlocked — the commit registry
/// then falls back to the env-var store, and a missing credential surfaces lazily as a clear
/// per-leg auth error rather than a panic. Never logs the passphrase.
#[must_use]
pub fn open_store_for_commit() -> Option<LocalStore> {
    open_store().ok().map(|(store, _)| store)
}

/// The persisted active account name for `driver`, read from the plaintext `<credentials>.active`
/// sidecar (selectors only — no secret, so no passphrase is needed to read it). This is the same
/// selection `qfs account use <driver> <account>` writes; the commit resolver consumes it to pick
/// which credential to apply with. Returns `None` when unset/unreadable.
#[must_use]
pub fn active_account(driver: &str) -> Option<String> {
    let cred = default_credentials_path()?;
    let body = std::fs::read_to_string(active_path(&cred)).ok()?;
    body.lines().find_map(|line| {
        let mut it = line.splitn(2, '\t');
        match (it.next(), it.next()) {
            (Some(d), Some(acct)) if d == driver && !acct.trim().is_empty() => {
                Some(acct.trim().to_string())
            }
            _ => None,
        }
    })
}

/// `<credentials>.salt` — the per-store KDF salt sidecar.
fn salt_path(cred: &std::path::Path) -> PathBuf {
    let mut p = cred.as_os_str().to_owned();
    p.push(".salt");
    PathBuf::from(p)
}

/// `<credentials>.active` — the persistent `account use` selection sidecar.
fn active_path(cred: &std::path::Path) -> PathBuf {
    let mut p = cred.as_os_str().to_owned();
    p.push(".active");
    PathBuf::from(p)
}

fn cred_key(driver: &str, account: &str) -> Result<CredentialKey, String> {
    let acct = AccountId::new(account).map_err(|e| format!("invalid account name: {e:?}"))?;
    Ok(CredentialKey::new(DriverId(driver.to_string()), acct))
}

fn run_inner(action: &AccountAction) -> Result<String, String> {
    match action {
        AccountAction::Add { driver, account } => {
            let (store, _) = open_store()?;
            let key = cred_key(driver, account)?;
            // The credential value comes from stdin — never argv.
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("reading the secret from stdin: {e}"))?;
            let value = buf.trim_end_matches(['\n', '\r']).to_string();
            if value.is_empty() {
                return Err(
                    "no secret on stdin — pipe it, e.g. `printf %s \"$TOKEN\" | qfs account add mail work`"
                        .into(),
                );
            }
            store
                .put(&key, Secret::from(value))
                .map_err(|e| format!("storing the credential: {e}"))?;
            Ok(format!("stored credential for {driver}/{account}"))
        }
        AccountAction::List { driver } => {
            let (store, _) = open_store()?;
            let filter = driver.as_ref().map(|d| DriverId(d.clone()));
            let recs = store
                .list(filter.as_ref())
                .map_err(|e| format!("listing accounts: {e}"))?;
            if recs.is_empty() {
                return Ok("no accounts configured".into());
            }
            // Selectors + metadata only — never a credential.
            for r in &recs {
                println!("{}/{}\t{}", r.driver.0, r.account, r.created_at);
            }
            Ok(format!("{} account(s)", recs.len()))
        }
        AccountAction::Remove { driver, account } => {
            let (store, _) = open_store()?;
            let key = cred_key(driver, account)?;
            store
                .remove(&key)
                .map_err(|e| format!("removing the credential: {e}"))?;
            Ok(format!("removed {driver}/{account} (idempotent)"))
        }
        AccountAction::Use { driver, account } => {
            // Validate the names, then persist the active selection beside the vault. (The commit
            // resolver consumes this once the commit path is wired — tracked in the execution
            // ticket; persisting it now is honest and forward-compatible.)
            let _ = cred_key(driver, account)?;
            let cred = default_credentials_path()
                .ok_or("cannot determine the credentials path (set HOME or XDG_CONFIG_HOME)")?;
            set_active(&active_path(&cred), driver, account)?;
            Ok(format!("active account for {driver} set to {account}"))
        }
    }
}

/// Replace (or add) the active account for `driver` in the `<credentials>.active` sidecar — one
/// `driver<TAB>account` line per driver, written owner-only (`0600`).
fn set_active(path: &std::path::Path, driver: &str, account: &str) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            let keep = l.split('\t').next() != Some(driver);
            keep && !l.trim().is_empty()
        })
        .map(str::to_string)
        .collect();
    lines.push(format!("{driver}\t{account}"));
    lines.sort();
    let body = format!("{}\n", lines.join("\n"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(path, body.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    owner_only(path);
    Ok(())
}

#[cfg(unix)]
fn owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn owner_only(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_and_active_sidecars_sit_beside_the_vault() {
        let cred = std::path::Path::new("/x/qfs/credentials");
        assert_eq!(salt_path(cred), PathBuf::from("/x/qfs/credentials.salt"));
        assert_eq!(
            active_path(cred),
            PathBuf::from("/x/qfs/credentials.active")
        );
    }

    #[test]
    fn cred_key_rejects_an_invalid_account_name() {
        assert!(cred_key("mail", "").is_err());
        let k = cred_key("mail", "work").expect("valid");
        assert_eq!(k.driver.0, "mail");
        assert_eq!(k.account.as_str(), "work");
    }

    #[test]
    fn set_active_replaces_one_driver_keeps_others_and_is_0600() {
        let dir = std::env::temp_dir().join(format!("qfs-active-test-{}", std::process::id()));
        let path = dir.join("credentials.active");
        let _ = std::fs::remove_dir_all(&dir);

        set_active(&path, "mail", "work").unwrap();
        set_active(&path, "s3", "prod").unwrap();
        // Replacing mail's account must NOT duplicate the mail line.
        set_active(&path, "mail", "personal").unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per driver: {lines:?}");
        assert!(
            lines.contains(&"mail\tpersonal"),
            "mail replaced: {lines:?}"
        );
        assert!(lines.contains(&"s3\tprod"), "s3 kept: {lines:?}");
        assert!(
            !lines.contains(&"mail\twork"),
            "old mail line gone: {lines:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o077;
            assert_eq!(mode, 0, "active sidecar is owner-only (0600)");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
