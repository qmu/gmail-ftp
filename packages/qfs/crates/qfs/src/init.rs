//! ADR 0008 §2 — **`qfs init`**, the first-run wizard (EPIC 20260702120000 / ticket
//! 20260702120030). One command readies a fresh machine: it creates the **vault** (the
//! envelope-encrypted credential store, via the KeyGuardian passphrase flow) and registers the
//! **operator identity** — replacing `qfs identity signup`.
//!
//! ## No password — by design
//! Local authentication is the OS login: whoever can run the binary under this OS user *is* the
//! operator, and a password the CLI never verifies would be a liability, not security (the old
//! signup collected exactly that). The operator's email is an **accountability label**. The local
//! account row is created with an **unusable** password hash (32 CSPRNG bytes, hashed and
//! discarded), so nothing can authenticate with it on the verified surfaces (the t46 HTTP
//! sessions) until a real password is set there — fail closed, no secret collected.
//!
//! ## One `$HOME` = one operator (the invariant)
//! A second `init` with the SAME email is idempotent (reports what exists, exit 0 — safe to
//! re-run). A second `init` with a DIFFERENT email is refused with an actionable error: teams
//! never share a `$HOME`; they meet on a server host (ADR 0008 §1). This removes the old
//! second-signup cliff (`SoleUser::Many` bricking every cloud bind with no recovery).

use qfs_cmd::InitAction;
use qfs_identity::{hash_password, validate_email, IdentityStore, Secret, SoleUser};

/// The injected init launcher. Returns the process exit code (`0` ok — including the idempotent
/// re-run — `1` on a structured, secret-free error). Never panics.
#[must_use]
pub fn run_init(action: &InitAction) -> i32 {
    match run_inner(action.email.as_deref()) {
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

fn run_inner(email: Option<&str>) -> Result<String, String> {
    // 1. The operator identity (System DB). Resolve the current state FIRST so a re-run is
    //    idempotent and a different-email attempt is refused before any prompt.
    let store = crate::identity::open_identity_store()?;
    let existing = match store
        .sole_user()
        .map_err(|e| format!("checking the operator identity: {e}"))?
    {
        SoleUser::One(u) => Some(u.primary_email),
        SoleUser::None => None,
        // Pre-invariant System DBs (multiple signups before ADR 0008) fail closed with the same
        // guidance the ADR gives: one operator per $HOME.
        SoleUser::Many => {
            return Err(
                "this host has multiple identities from before the one-operator rule — qfs is \
                 one operator per OS user (teams meet on a server host). Remove the extra \
                 identities or use a separate $HOME per person"
                    .into(),
            )
        }
    };

    let operator = match (existing, email) {
        // Idempotent re-run: same email (or none given) reports what exists.
        (Some(current), None) => current,
        (Some(current), Some(given)) if given == current => current,
        (Some(current), Some(given)) => {
            return Err(format!(
                "this host's operator is {current} — one operator per OS user (ADR 0008). To act \
                 as {given}, use a separate OS user / $HOME, or a server host (teams never share \
                 a $HOME)"
            ));
        }
        // Fresh host: take the email from argv, else prompt on a terminal.
        (None, given) => {
            let email = match given {
                Some(e) => e.to_string(),
                None if crate::tty::is_interactive() => {
                    eprintln!(
                        "Welcome to qfs. This registers you as this machine's operator — the \
                         email is an accountability label (not a login; your OS user is the \
                         authentication)."
                    );
                    crate::tty::prompt_line("Operator email: ")?
                }
                None => {
                    return Err(
                        "no email — run `qfs init <email>` (non-interactive), or run it in a \
                         terminal to be prompted"
                            .into(),
                    )
                }
            };
            validate_email(&email).map_err(|e| e.to_string())?;
            // An UNUSABLE password hash (no password is collected — see the module doc): 32
            // CSPRNG bytes, hashed, plaintext dropped. Nothing can verify against it.
            let unusable = Secret::from(qfs_secrets::generate_dek().to_vec());
            let hash =
                hash_password(&unusable).map_err(|e| format!("hashing the placeholder: {e}"))?;
            let user = store
                .signup_local(&email, &hash)
                .map_err(|e| format!("registering the operator: {e}"))?;
            user.primary_email
        }
    };

    // 2. The vault (Project DB). `open_store` runs the guardian flow: a fresh store walks the
    //    passphrase creation (confirm-twice on a terminal, `QFS_PASSPHRASE` in automation) and
    //    enrolls it as slot #1; an existing store just unlocks (keychain first if enrolled).
    let vault = crate::connection::open_store()?;
    let slots = vault
        .list_slots()
        .map_err(|e| format!("listing the vault key slots: {e}"))?;
    let kinds: Vec<&str> = slots.iter().map(|(_, kind, _)| kind.as_str()).collect();

    Ok(format!(
        "qfs is ready: operator {operator}; vault unlocked ({} key slot(s): {})",
        slots.len(),
        kinds.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Env-mutating tests serialize on the crate-wide ENV_LOCK (shared with store.rs/oauth.rs).
    use crate::ENV_LOCK;

    /// Drive `run_inner` against a fresh, isolated config home. Non-interactive by construction
    /// (the test harness pipes stdio), with the passphrase from the env — the automation path.
    fn with_fresh_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_pass = std::env::var_os("QFS_PASSPHRASE");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::set_var("QFS_PASSPHRASE", "init-test-pass");
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

    /// The full non-interactive first run: operator + vault in one command; the re-run is
    /// idempotent (exit-0 semantics: `Ok`), and a DIFFERENT email is refused with the
    /// one-operator error (ADR 0008 §2 — the old second-signup cliff is gone).
    #[test]
    fn init_is_idempotent_and_enforces_one_operator() {
        with_fresh_home(|| {
            let first = run_inner(Some("op@example.com")).expect("first init succeeds");
            assert!(first.contains("op@example.com"), "reports the operator");
            assert!(first.contains("passphrase"), "reports the enrolled slot");

            // Idempotent: same email, and no email at all, both report-and-succeed.
            assert!(run_inner(Some("op@example.com")).is_ok());
            assert!(run_inner(None).is_ok(), "a bare re-run is safe");

            // The invariant: a different email is refused, actionably.
            let err = run_inner(Some("other@example.com")).unwrap_err();
            assert!(
                err.contains("op@example.com") && err.contains("one operator"),
                "the refusal names the existing operator and the rule: {err}"
            );
        });
    }

    /// Fresh host, no email, no terminal: a clear usage error (never a hang, never a prompt in a
    /// pipe), and nothing is created.
    #[test]
    fn init_without_email_non_interactive_is_a_clear_error() {
        with_fresh_home(|| {
            let err = run_inner(None).unwrap_err();
            assert!(err.contains("qfs init <email>"), "actionable: {err}");
            // Nothing was half-created: a follow-up init with an email still runs the full path.
            assert!(run_inner(Some("op@example.com")).is_ok());
        });
    }

    /// A malformed email fails validation before any store write.
    #[test]
    fn init_rejects_a_malformed_email() {
        with_fresh_home(|| {
            assert!(run_inner(Some("not-an-email")).is_err());
            assert!(run_inner(Some("op@example.com")).is_ok());
        });
    }
}
