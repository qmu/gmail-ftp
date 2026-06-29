//! Pure sign-up validation: email **shape** and password **policy**. No I/O, no store — just total
//! functions over owned/borrowed input, unit-tested below. The binary runs these BEFORE touching the
//! store, so a malformed input fails fast with a structured, AI-legible error (never a panic, never a
//! DB round-trip).
//!
//! ## Scope (open product decisions — flagged, not silently baked)
//! - **Email** is validated for *shape* only — a single `@`, a non-empty local part, and a dotted
//!   domain. Real deliverability / verification is deferred to **M5 invites (t55)**; we do NOT send a
//!   confirmation here.
//! - **Password policy** is a minimum/maximum LENGTH (see [`MIN_PASSWORD_LEN`]/[`MAX_PASSWORD_LEN`]).
//!   A stronger strength meter and whether to allow passwordless-only deployments are open M2
//!   decisions; this is the floor, not the final policy.

use crate::Secret;

/// The minimum password length (bytes) accepted at sign-up. A deliberately modest floor — the real
/// defense is the argon2id KDF, not a draconian composition rule (an open M2 policy decision).
pub const MIN_PASSWORD_LEN: usize = 8;

/// The maximum password length (bytes). Bounds the work a single sign-up can ask argon2id to do over
/// the input, so an enormous pasted blob cannot be used to burn CPU/memory (a cheap DoS floor).
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Validate an email's **shape** (not its deliverability). Accepts a single `@` with a non-empty
/// local part and a dotted, non-empty domain, and rejects internal whitespace. Returns `Ok(())` on a
/// well-shaped address.
///
/// # Errors
/// [`SignupError::EmptyEmail`] if empty/blank, [`SignupError::InvalidEmail`] otherwise.
pub fn validate_email(email: &str) -> Result<(), SignupError> {
    let email = email.trim();
    if email.is_empty() {
        return Err(SignupError::EmptyEmail);
    }
    if email.chars().any(char::is_whitespace) {
        return Err(SignupError::InvalidEmail);
    }
    // Exactly one '@', splitting a non-empty local part from a dotted, non-empty domain.
    let Some((local, domain)) = email.split_once('@') else {
        return Err(SignupError::InvalidEmail);
    };
    if local.is_empty() || domain.contains('@') {
        return Err(SignupError::InvalidEmail);
    }
    // The domain must be dotted, with non-empty labels on both sides of every dot.
    if domain.starts_with('.')
        || domain.ends_with('.')
        || !domain.contains('.')
        || domain.split('.').any(str::is_empty)
    {
        return Err(SignupError::InvalidEmail);
    }
    Ok(())
}

/// Validate a candidate password against the length policy. The value is a [`Secret`]; we read only
/// its byte length (metadata, not the value), so nothing is exposed here.
///
/// # Errors
/// [`SignupError::PasswordTooShort`] / [`SignupError::PasswordTooLong`] with the offending bound.
pub fn validate_password(password: &Secret) -> Result<(), SignupError> {
    let len = password.len();
    if len < MIN_PASSWORD_LEN {
        return Err(SignupError::PasswordTooShort {
            min: MIN_PASSWORD_LEN,
        });
    }
    if len > MAX_PASSWORD_LEN {
        return Err(SignupError::PasswordTooLong {
            max: MAX_PASSWORD_LEN,
        });
    }
    Ok(())
}

/// A structured, secret-free sign-up validation failure. The password's *value* never appears (only
/// the policy bound it violated); the email *is* echoable (it is a handle, not a secret).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignupError {
    /// The email was empty or whitespace-only.
    #[error("email must not be empty")]
    EmptyEmail,
    /// The email was not a well-shaped address (single `@`, non-empty local part, dotted domain).
    #[error("email is not a valid address (need user@dotted.domain)")]
    InvalidEmail,
    /// The password was shorter than [`MIN_PASSWORD_LEN`].
    #[error("password is too short (minimum {min} characters)")]
    PasswordTooShort {
        /// The minimum length that was not met.
        min: usize,
    },
    /// The password exceeded [`MAX_PASSWORD_LEN`].
    #[error("password is too long (maximum {max} characters)")]
    PasswordTooLong {
        /// The maximum length that was exceeded.
        max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_shaped_emails_pass() {
        for ok in ["a@b.com", "user.name@sub.example.co.jp", "  trimmed@x.io  "] {
            assert!(validate_email(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn malformed_emails_are_rejected() {
        assert_eq!(validate_email(""), Err(SignupError::EmptyEmail));
        assert_eq!(validate_email("   "), Err(SignupError::EmptyEmail));
        for bad in [
            "no-at-sign",
            "@nolocal.com",
            "user@",
            "user@nodot",
            "user@.leadingdot",
            "user@trailingdot.",
            "a@b@c.com",
            "spa ce@x.com",
            "user@dou..ble.com",
        ] {
            assert_eq!(
                validate_email(bad),
                Err(SignupError::InvalidEmail),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn password_length_policy_is_enforced() {
        assert_eq!(
            validate_password(&Secret::from("short")),
            Err(SignupError::PasswordTooShort { min: 8 })
        );
        assert!(validate_password(&Secret::from("just-enough!")).is_ok());

        let huge = "x".repeat(MAX_PASSWORD_LEN + 1);
        assert_eq!(
            validate_password(&Secret::from(huge)),
            Err(SignupError::PasswordTooLong {
                max: MAX_PASSWORD_LEN
            })
        );
        // Exactly at the bounds is accepted.
        assert!(validate_password(&Secret::from("x".repeat(MIN_PASSWORD_LEN))).is_ok());
        assert!(validate_password(&Secret::from("x".repeat(MAX_PASSWORD_LEN))).is_ok());
    }
}
