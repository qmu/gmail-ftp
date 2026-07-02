//! The `qfs identity` composition root (t45): the System-DB-backed identity store I/O behind
//! `qfs identity signup` / `qfs identity whoami`, injected into `qfs-cmd` as the
//! [`qfs_cmd::IdentityLauncher`].
//!
//! `qfs-cmd` may not depend on the concrete `qfs-store` / `qfs-identity` backends (the dep_direction
//! guard), so — exactly like the connection launcher — the binary owns this and `qfs-cmd` only parses
//! the verb and calls in. The binary is also the one crate that resolves a real DB path (decision F).
//!
//! ## Scope + security (decision §4.1, RFD §10)
//! - AUTHENTICATION ONLY: sign-up creates a `users` row + a `local` password account. A signed-up
//!   user can do NOTHING privileged yet — there is **no session** (t46) and no authorization (M2).
//! - The password is read from **STDIN**, never argv (argv leaks into shell history and `ps`), and
//!   carried as a [`Secret`] (zeroized on drop after hashing).
//! - `whoami` prints only the email + user id — **never** the password hash. The hash is never
//!   logged, never returned, never serialized into an audit event.

use std::io::Read;

use qfs_cmd::IdentityAction;
use qfs_identity::{
    hash_password, validate_email, validate_password, IdentityStore, Secret, SoleUser,
};
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
            "no password on stdin — pipe it, e.g. `printf %s \"$PW\" | qfs identity signup a@b.com`"
                .into(),
        );
    }
    Ok(Secret::from(trimmed))
}

fn run_inner(action: &IdentityAction) -> Result<String, String> {
    match action {
        IdentityAction::Signup { email } => {
            // Validate the email shape BEFORE touching stdin / the store (fail fast, no DB round-trip).
            validate_email(email).map_err(|e| e.to_string())?;
            // The password comes from stdin — never argv — as a zeroized Secret.
            let password = read_password_from_stdin()?;
            validate_password(&password).map_err(|e| e.to_string())?;
            // Hash with argon2id; the plaintext `password` is dropped (zeroized) at the end of this
            // arm. The store only ever sees the PasswordHash, never the plaintext.
            let hash =
                hash_password(&password).map_err(|e| format!("hashing the password: {e}"))?;
            let store = open_identity_store()?;
            let user = store
                .signup_local(email, &hash)
                .map_err(|e| format!("signing up: {e}"))?;
            // Confirmation prints the email + id only — NEVER the hash.
            Ok(format!(
                "signed up {} as user {} (local sign-up; no session yet)",
                user.primary_email, user.id
            ))
        }
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
                        Ok("no users yet — run `qfs identity signup <email>`".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signup_rejects_a_malformed_email_before_any_io() {
        // A bad email fails at validation (no stdin read, no store open).
        let action = IdentityAction::Signup {
            email: "not-an-email".into(),
        };
        let code = run_identity(&action);
        assert_eq!(code, 1, "a malformed email is a structured error (exit 1)");
    }
}
