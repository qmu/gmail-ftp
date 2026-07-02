//! The `qfs identity` composition root (t45): the System-DB-backed identity store I/O behind
//! `qfs identity whoami`, injected into `qfs-cmd` as the [`qfs_cmd::IdentityLauncher`].
//! (Signing up moved to `qfs init` — ADR 0008 §2 retired the unverified-password signup;
//! see [`crate::init`].)
//!
//! `qfs-cmd` may not depend on the concrete `qfs-store` / `qfs-identity` backends (the dep_direction
//! guard), so — exactly like the connection launcher — the binary owns this and `qfs-cmd` only parses
//! the verb and calls in. The binary is also the one crate that resolves a real DB path (decision F).
//!
//! ## Scope + security (decision §4.1, RFD §10)
//! - AUTHENTICATION ONLY: there is **no session** (t46) and no authorization (M2).
//! - [`read_password_from_stdin`] (kept for the t55 invite redeem, which DOES set a real
//!   password): STDIN or an echo-off TTY prompt, never argv; carried as a [`Secret`].
//! - `whoami` prints only the email + user id — **never** a password hash. The hash is never
//!   logged, never returned, never serialized into an audit event.

use std::io::Read;

use qfs_cmd::IdentityAction;
use qfs_identity::{IdentityStore, Secret, SoleUser};
use qfs_store::SqliteIdentityStore;

/// The injected identity launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_identity(action: &IdentityAction) -> i32 {
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

/// Open the System DB at the default path and build the identity store over its owned, migrated
/// connection (the t45 seam — `SystemDb::into_db().into_connection()`). The identity migration (v3)
/// is applied by `SystemDb::open`.
pub(crate) fn open_identity_store() -> Result<SqliteIdentityStore, String> {
    let sys = crate::store::open_system_db()
        .map_err(|e| format!("opening the system database: {e}"))?
        .ok_or("cannot determine the system database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(SqliteIdentityStore::from_db(sys.into_db()))
}

/// Read the password being SET as a [`Secret`] (never argv). A human at a terminal is PROMPTED
/// (echo off, confirmed twice so a typo can't lock them out); automation keeps the stdin path —
/// `printf %s "$PW" | …` — trimming a single trailing newline and rejecting an empty password.
/// Shared with the t55 invite-redeem launcher (which sets the redeemer's password the same way).
pub(crate) fn read_password_from_stdin() -> Result<Secret, String> {
    if crate::tty::stdin_is_terminal() {
        return crate::tty::prompt_secret_confirmed("Choose a password: ", "Confirm password: ");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("reading the password from stdin: {e}"))?;
    let trimmed = buf.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(
            "no password on stdin — pipe it, e.g. `printf %s \"$PW\" | qfs invite redeem <token>`"
                .into(),
        );
    }
    Ok(Secret::from(trimmed))
}

fn run_inner(action: &IdentityAction) -> Result<String, String> {
    match action {
        IdentityAction::Whoami { email } => {
            let store = open_identity_store()?;
            match email {
                // An explicit email: look that user up.
                Some(e) => match store
                    .find_user_by_email(e)
                    .map_err(|err| format!("looking up the user: {err}"))?
                {
                    Some(u) => Ok(format!("{} (user {})", u.primary_email, u.id)),
                    None => Ok(format!("no user is signed up for {e}")),
                },
                // No email + no session yet (t46): resolve the sole user, if there is exactly one.
                None => match store
                    .sole_user()
                    .map_err(|err| format!("resolving the current user: {err}"))?
                {
                    SoleUser::One(u) => Ok(format!("{} (user {})", u.primary_email, u.id)),
                    SoleUser::None => {
                        Ok("no users yet — run `qfs init`".to_string())
                    }
                    SoleUser::Many => Ok(
                        "multiple users on this host and no session yet — specify `qfs identity whoami <email>`"
                            .to_string(),
                    ),
                },
            }
        }
    }
}
