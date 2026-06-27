//! The `qfs connection` composition root: the real credential-store I/O that backs
//! `qfs connection add/list/use/remove`, injected into `qfs-cmd` as the [`qfs_cmd::ConnectionLauncher`].
//!
//! `qfs-cmd` may not depend on the concrete `qfs-secrets` backend (the dep_direction guard), so —
//! exactly like the shell / serve / describe launchers — the binary owns this and `qfs-cmd` only
//! parses the verb and calls in.
//!
//! ## Security (RFD §10)
//! - The credential **value** is read from **stdin**, never from argv (argv leaks into shell
//!   history and `ps`).
//! - Credentials live in the envelope-encrypted SQLite **Project DB** ([`crate::secret_store`]):
//!   a random data-key (DEK) encrypts each secret value (ChaCha20-Poly1305), and the DEK is wrapped
//!   under a key derived from the `QFS_PASSPHRASE` env var (argon2id) — the t43 replacement for the
//!   old file vault. The active-connection selection lives in the DB's `active_account` table (no
//!   passphrase needed — selectors only). Secrets are never printed, logged, or echoed.

use std::io::Read;

use qfs_cmd::ConnectionAction;
use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secret, Secrets};
use rusqlite::Connection;

use crate::secret_store::{self, SqliteSecrets};

/// The injected connection launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_connection(action: &ConnectionAction) -> i32 {
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

/// Open the migrated Project DB and return its **owned** connection (the t42 seam). The connection
/// carries the t43 secret-store schema; callers either move it into [`SqliteSecrets`] (the credential
/// path) or use it directly for the passphrase-free `active_account` table.
fn open_project_conn() -> Result<Connection, String> {
    let proj = crate::store::open_project_db()
        .map_err(|e| format!("opening the project database: {e}"))?
        .ok_or("cannot determine the project database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(proj.into_db().into_connection())
}

/// Open the envelope-encrypted SQLite credential store: open + migrate the Project DB, then unlock
/// (or initialize) the envelope with `QFS_PASSPHRASE`.
fn open_store() -> Result<SqliteSecrets, String> {
    let conn = open_project_conn()?;
    let pass = std::env::var("QFS_PASSPHRASE").map_err(|_| {
        "QFS_PASSPHRASE is not set — export it to unlock the encrypted credential store".to_string()
    })?;
    if pass.is_empty() {
        return Err("QFS_PASSPHRASE is empty".into());
    }
    SqliteSecrets::open_or_init(conn, &Secret::from(pass))
        .map_err(|e| format!("opening the credential store: {e}"))
}

/// Open the credential store for the **commit resolver** (read path): the same envelope-encrypted
/// SQLite store `connection add` writes to, when `QFS_PASSPHRASE` + the Project DB are both available.
/// Returns `None` (best-effort, never an error) when the store cannot be unlocked — the commit
/// registry then falls back to the env-var store, and a missing credential surfaces lazily as a
/// clear per-leg auth error rather than a panic. Never logs the passphrase.
#[must_use]
pub fn open_store_for_commit() -> Option<SqliteSecrets> {
    open_store().ok()
}

/// The persisted active connection name for `driver`, read from the Project DB's `active_account`
/// table (selectors only — no secret, so no passphrase is needed to read it). This is the same
/// selection `qfs connection use <driver> <connection>` writes; the commit resolver consumes it to
/// pick which credential to apply with. Returns `None` when unset/unreadable.
#[must_use]
pub fn active_connection(driver: &str) -> Option<String> {
    let conn = open_project_conn().ok()?;
    secret_store::db_get_active(&conn, driver)
}

fn cred_key(driver: &str, connection: &str) -> Result<CredentialKey, String> {
    let conn_id =
        ConnectionId::new(connection).map_err(|e| format!("invalid connection name: {e:?}"))?;
    Ok(CredentialKey::new(DriverId(driver.to_string()), conn_id))
}

fn run_inner(action: &ConnectionAction) -> Result<String, String> {
    match action {
        ConnectionAction::Add { driver, connection } => {
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            // The credential value comes from stdin — never argv.
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("reading the secret from stdin: {e}"))?;
            let value = buf.trim_end_matches(['\n', '\r']).to_string();
            if value.is_empty() {
                return Err(
                    "no secret on stdin — pipe it, e.g. `printf %s \"$TOKEN\" | qfs connection add mail work`"
                        .into(),
                );
            }
            store
                .put(&key, Secret::from(value))
                .map_err(|e| format!("storing the credential: {e}"))?;
            Ok(format!("stored credential for {driver}/{connection}"))
        }
        ConnectionAction::List { driver } => {
            let store = open_store()?;
            let filter = driver.as_ref().map(|d| DriverId(d.clone()));
            let recs = store
                .list(filter.as_ref())
                .map_err(|e| format!("listing connections: {e}"))?;
            if recs.is_empty() {
                return Ok("no connections configured".into());
            }
            // Selectors + metadata only — never a credential.
            for r in &recs {
                println!("{}/{}\t{}", r.driver.0, r.connection, r.created_at);
            }
            Ok(format!("{} connection(s)", recs.len()))
        }
        ConnectionAction::Remove { driver, connection } => {
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            store
                .remove(&key)
                .map_err(|e| format!("removing the credential: {e}"))?;
            Ok(format!("removed {driver}/{connection} (idempotent)"))
        }
        ConnectionAction::Use { driver, connection } => {
            // Validate the names, then persist the active selection into the Project DB's
            // `active_account` table (selectors only — no passphrase needed). The commit resolver
            // reads it back via `active_connection()`.
            let _ = cred_key(driver, connection)?;
            let conn = open_project_conn()?;
            secret_store::db_set_active(&conn, driver, connection)
                .map_err(|e| format!("setting the active connection: {e}"))?;
            Ok(format!(
                "active connection for {driver} set to {connection}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_key_rejects_an_invalid_connection_name() {
        assert!(cred_key("mail", "").is_err());
        let k = cred_key("mail", "work").expect("valid");
        assert_eq!(k.driver.0, "mail");
        assert_eq!(k.connection.as_str(), "work");
    }

    /// The active-connection selection now round-trips through the Project DB's `active_account`
    /// table (replacing the old `.active` sidecar): `use` UPSERTs, the resolver reads back, and
    /// per-driver rows stay independent (last-writer-wins). Exercised over the same DB seam the
    /// binary uses (`db_set_active` / `db_get_active`).
    #[test]
    fn active_selection_round_trips_through_the_db_table() {
        use qfs_store::{MemorySource, ProjectDb};
        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();

        assert!(secret_store::db_get_active(&conn, "mail").is_none());
        secret_store::db_set_active(&conn, "mail", "work").unwrap();
        secret_store::db_set_active(&conn, "s3", "prod").unwrap();
        // Replacing mail's connection must NOT affect s3 and must not duplicate the row.
        secret_store::db_set_active(&conn, "mail", "personal").unwrap();

        assert_eq!(
            secret_store::db_get_active(&conn, "mail").as_deref(),
            Some("personal")
        );
        assert_eq!(
            secret_store::db_get_active(&conn, "s3").as_deref(),
            Some("prod")
        );
    }
}
