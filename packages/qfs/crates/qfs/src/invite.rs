//! The `qfs invite` composition root (t55, roadmap M5): the System-DB-backed [`SqliteInviteStore`]
//! I/O plus the binary-owned CSPRNG that mints the one-time invite token, behind
//! `qfs invite create/redeem/revoke`. Injected into `qfs-cmd` as the [`qfs_cmd::InviteLauncher`].
//!
//! `qfs-cmd` / the spine may not depend on the concrete `qfs-store` / `qfs-identity` backends (the
//! dep_direction guard), so — exactly like the identity / session launchers — the binary owns this.
//! The binary is also the one crate that resolves a real DB path (decision F) AND the one leaf that
//! may own a CSPRNG: it mints the opaque invite token from OS entropy here and hands `qfs-identity`
//! only the random bytes, keeping that domain core deterministic/testable.
//!
//! ## Scope + security (decision §4.1, RFD §10)
//! - **MEMBERSHIP, not authorization.** Redeeming an invite creates a real local identity (the t45
//!   `users` + `local` `accounts` rows, argon2id password) and a `memberships` row that says the user
//!   *belongs* to the host — it grants NO capability (the ACL is t57). Do not read "is a member" as a
//!   permission.
//! - **One-time token hygiene.** The token is generated here from a CSPRNG, returned to the operator
//!   exactly once (the one-time URL), and persisted ONLY as `sha256_hex(token)`. Redemption verifies
//!   the digest constant-time, is single-use (burned atomically), expiring, and revocable. The
//!   plaintext token is never logged — only the single create-time print reveals it.
//! - **Password from STDIN.** At redeem the password is read from STDIN, never argv (argv leaks into
//!   shell history / `ps`), carried as a [`Secret`] (zeroized after hashing).
//!
//! ## Documented seams (honest scope)
//! - **Email delivery is a SEAM, not a claim.** When the host is configured for outbound mail, an
//!   invite email is an ordinary qfs effect (a `CALL mail.send(...)` plan through the normal commit
//!   boundary). That wiring is NOT built here; this launcher generates the invite + token and surfaces
//!   the one-time URL as the testable fallback (no silent failure).
//! - **The HTTP accept route is a SEAM.** Establishing a t46 session on redeem belongs to the
//!   `crates/http` accept endpoint (the binding the SPA hits); the CLI redeem creates the identity +
//!   membership and leaves session issuance to that route.

use qfs_cmd::InviteAction;
use qfs_identity::{
    hash_password, validate_email, validate_password, InviteStore, InviteToken, MembershipScope,
    NewInvite, Role,
};
use qfs_store::SqliteInviteStore;
use rand::RngCore;

/// The one-time token's entropy width in bytes (256 bits → a 64-char lowercase-hex token) — the same
/// width as the t46 session token; comfortably beyond brute-force/birthday reach for a bearer secret.
const TOKEN_ENTROPY_BYTES: usize = 32;

/// The default invite lifetime in seconds (7 days) when `--ttl` is omitted. An invite is a short-lived
/// handout, not a standing credential; a week is the least-surprising default for the milestone. The
/// operator overrides it per-invite via `--ttl`.
const DEFAULT_INVITE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// The injected invite launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_invite(action: &InviteAction) -> i32 {
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

/// Open the System DB at the default path and build the invite store over its owned, migrated
/// connection (the t55 seam — `SystemDb::into_db().into_connection()`). The invites migration (v8) is
/// applied by `SystemDb::open`.
fn open_invite_store() -> Result<SqliteInviteStore, String> {
    let sys = crate::store::open_system_db()
        .map_err(|e| format!("opening the system database: {e}"))?
        .ok_or("cannot determine the system database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(SqliteInviteStore::from_db(sys.into_db()))
}

/// Mint a fresh one-time invite token from OS entropy (the binary owns the CSPRNG). The raw bytes are
/// handed to [`InviteToken::from_entropy`]; the returned token's plaintext lives only inside the
/// redacting [`qfs_identity::Secret`] until it is printed once.
fn generate_token() -> InviteToken {
    let mut entropy = [0u8; TOKEN_ENTROPY_BYTES];
    rand::rng().fill_bytes(&mut entropy);
    InviteToken::from_entropy(&entropy)
}

/// Parse the optional `--scope` flag into a [`MembershipScope`] (default host). An unknown value is a
/// structured error rather than a silent fallback (the operator should know their invite's scope).
fn parse_scope(scope: Option<&str>) -> Result<MembershipScope, String> {
    match scope {
        None | Some("host") => Ok(MembershipScope::Host),
        Some("project") => Ok(MembershipScope::Project),
        Some(other) => Err(format!("unknown scope '{other}' (use 'host' or 'project')")),
    }
}

/// Parse the optional `--role` flag into a [`Role`] (default member). An unknown value is a structured
/// error (fail toward least privilege explicitly rather than silently demoting to member).
fn parse_role(role: Option<&str>) -> Result<Role, String> {
    match role {
        None | Some("member") => Ok(Role::Member),
        Some("admin") => Ok(Role::Admin),
        Some("owner") => Ok(Role::Owner),
        Some(other) => Err(format!(
            "unknown role '{other}' (use 'member', 'admin', or 'owner')"
        )),
    }
}

fn run_inner(action: &InviteAction) -> Result<String, String> {
    match action {
        InviteAction::Create {
            email,
            scope,
            project,
            role,
            ttl_secs,
        } => {
            // Validate an explicit email's shape up front (it is the optional delivery handle).
            if let Some(e) = email {
                validate_email(e).map_err(|err| err.to_string())?;
            }
            let scope = parse_scope(scope.as_deref())?;
            let role = parse_role(role.as_deref())?;
            let ttl_secs = ttl_secs.unwrap_or(DEFAULT_INVITE_TTL_SECS);
            if ttl_secs <= 0 {
                return Err("--ttl must be a positive number of seconds".into());
            }
            // Mint the token (CSPRNG), store ONLY its digest, and return the raw token once.
            let token = generate_token();
            let new = NewInvite {
                email: email.clone(),
                scope,
                project: project.clone(),
                role,
                ttl_secs,
                created_by: None,
            };
            let store = open_invite_store()?;
            let invite = store
                .create_invite(&new, &token.hash())
                .map_err(|e| format!("creating the invite: {e}"))?;
            // The ONE-TIME reveal: print the token exactly here, never again, never logged.
            let raw = token
                .reveal()
                .expose_str()
                .ok_or("the minted token was not valid UTF-8")?;
            Ok(format!(
                "invite {} created (expires {}, scope {}, role {}).\n\
                 one-time token (store it now — it is shown only once): {}\n\
                 redeem with: printf %s \"$PASSWORD\" | qfs invite redeem {} <email>\n\
                 (email delivery is a seam; hand out this URL/token out of band until mail is wired)",
                invite.id, invite.expires_at, invite.scope, invite.role, raw, raw
            ))
        }
        InviteAction::Redeem { token, email } => {
            // Validate the redeemer's email shape BEFORE touching stdin / the store (fail fast).
            validate_email(email).map_err(|e| e.to_string())?;
            // The password comes from stdin — never argv — as a zeroized Secret.
            let password = crate::identity::read_password_from_stdin()?;
            validate_password(&password).map_err(|e| e.to_string())?;
            let hash =
                hash_password(&password).map_err(|e| format!("hashing the password: {e}"))?;
            // The presented token is hashed; only its digest is matched against the store.
            let token_hash = InviteToken::from_redeem_value(token).hash();
            let store = open_invite_store()?;
            let redemption = store
                .accept_invite(&token_hash, email, &hash)
                .map_err(|e| format!("redeeming the invite: {e}"))?;
            // Confirmation prints the email + ids only — NEVER the token or the password hash. A t46
            // session is established by the HTTP accept route (a seam), not the CLI redeem.
            Ok(format!(
                "redeemed: {} is now user {} and a {} member (membership {})",
                redemption.user.primary_email,
                redemption.user.id,
                redemption.membership.role,
                redemption.membership.id
            ))
        }
        InviteAction::Revoke { id } => {
            let store = open_invite_store()?;
            let revoked = store
                .revoke_invite(qfs_identity::InviteId(*id))
                .map_err(|e| format!("revoking the invite: {e}"))?;
            if revoked {
                Ok(format!(
                    "invite {id} revoked (its token can no longer redeem)"
                ))
            } else {
                Ok(format!(
                    "invite {id} was not revocable (unknown, already redeemed, or already revoked)"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_rejects_a_malformed_email_before_any_io() {
        // A bad email fails at validation (no store open).
        let code = run_invite(&InviteAction::Create {
            email: Some("not-an-email".into()),
            scope: None,
            project: None,
            role: None,
            ttl_secs: None,
        });
        assert_eq!(code, 1, "a malformed email is a structured error (exit 1)");
    }

    #[test]
    fn create_rejects_an_unknown_scope_or_role() {
        assert!(parse_scope(Some("galaxy")).is_err());
        assert!(parse_role(Some("super-admin")).is_err());
        assert_eq!(parse_scope(None).unwrap(), MembershipScope::Host);
        assert_eq!(parse_role(None).unwrap(), Role::Member);
    }

    #[test]
    fn minted_tokens_are_unique_and_64_hex_chars() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.reveal().expose_str().unwrap().len(), 64);
        assert_ne!(a.hash(), b.hash(), "two minted invite tokens must differ");
    }
}
