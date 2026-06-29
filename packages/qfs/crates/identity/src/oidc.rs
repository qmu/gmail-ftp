//! t56 (roadmap **M5**): the **subject → local identity mapping** for upstream OIDC federation —
//! [`link_or_create_from_oidc`], the rule that turns a verified upstream ID token into a local
//! [`User`] + a linked [`Account`].
//!
//! This is the "hub" half of the federation story (decision D, §4.1): qfs is the single hub every
//! face trusts, and an UPSTREAM IdP (qfs Cloud / Google Workspace / Entra / Okta / a generic OIDC
//! provider) federates IN. A human signs in THROUGH the upstream, the binary verifies the upstream
//! ID token (signature against the upstream's JWKS + `iss`/`aud`/`exp`/`nonce` — that verification
//! lives in `qfs-oauth`, NOT here), and the resulting claims are handed to this linker, which:
//!
//! 1. recognises a returning federated user by the unique `(provider, subject)` account row, or
//! 2. links a NEW federated account onto an EXISTING local user matched by a **verified** email, or
//! 3. provisions a brand-new local user + the federated account.
//!
//! ## Verify, never trust (the load-bearing security rule)
//! The `(provider, subject)` pair is the trust anchor: the upstream's signature already proved the
//! subject, so a returning subject is linked regardless of the email claim. But linking by EMAIL —
//! attaching a federated login to an *existing* local user — is an account-takeover vector, so it is
//! gated on `email_verified == true`. An UNVERIFIED email never links to (or provisions) anything:
//! it fails closed with [`LinkError::UnverifiedEmail`]. We never trust an unverified claim to bind
//! one human's federated login to another human's local identity.
//!
//! ## Identity, not authorization (§4.1)
//! A freshly-federated user gets an identity + (later) a membership — it grants ZERO capability. A
//! linked account is default-deny until `POLICY`/membership (t55/t57). "Came from a trusted IdP"
//! never implies "may do X".
//!
//! ## A leaf, over the injected store
//! This is pure domain logic over the consumer-side [`IdentityStore`] trait; the rusqlite impl is
//! injected by the binary (`qfs-store`). It pulls in no I/O of its own.

use crate::signup::validate_email;
use crate::store::{IdentityError, IdentityStore};
use crate::{SignupError, User};

/// The verified upstream identity claims the linker consumes — the minimal projection of an upstream
/// OIDC ID token AFTER `qfs-oauth` has verified its signature + `iss`/`aud`/`exp`/`nonce`. It carries
/// no token and no secret; just the provider-scoped identifiers and the email-verification bit the
/// linking rule turns on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcClaims {
    /// The local provider key the upstream is registered under (e.g. its issuer URL, or an operator
    /// label like `google`). One half of the unique `(provider, subject)` account key — NOT the
    /// `local` password provider.
    pub provider: String,
    /// The upstream-scoped subject (`sub`) — stable per user at that IdP. The other half of the
    /// `(provider, subject)` key; the signature already proved it, so it is the trust anchor.
    pub subject: String,
    /// The email claim. Used to match/provision a local user ONLY when [`email_verified`] is true.
    ///
    /// [`email_verified`]: OidcClaims::email_verified
    pub email: String,
    /// Whether the upstream asserts the email is VERIFIED. Linking by email is gated on this; an
    /// unverified email fails closed (it never links to or provisions a user).
    pub email_verified: bool,
}

/// How a verified upstream login resolved to a local [`User`]. Distinct variants so the caller (and a
/// test) can tell a returning federated user from a first-time link from a fresh provision — and so a
/// "linked an existing local user" event can be audited differently from "created a new user".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The `(provider, subject)` account already existed — a returning federated user. The email
    /// claim is irrelevant here (the subject is the proven trust anchor).
    ExistingLink(User),
    /// No prior link existed, but a local user with the **verified** email did: a NEW federated
    /// account was linked onto that existing user (one human, a second sign-in face).
    LinkedToExistingUser(User),
    /// No prior link and no matching local user: a brand-new local user was provisioned and the
    /// federated account linked to it.
    CreatedNewUser(User),
}

impl LinkOutcome {
    /// The resolved local [`User`], whichever way it was reached (the caller establishes a session
    /// over this exactly as a local sign-in does — downstream code never distinguishes "how").
    #[must_use]
    pub fn user(&self) -> &User {
        match self {
            LinkOutcome::ExistingLink(u)
            | LinkOutcome::LinkedToExistingUser(u)
            | LinkOutcome::CreatedNewUser(u) => u,
        }
    }
}

/// A structured, secret-free federated-linking failure. Carries no token/claim value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// The upstream email is not verified, so it must not bind to (or provision) a local user — the
    /// account-takeover guard. The caller fails the login closed; a returning `(provider, subject)`
    /// is handled BEFORE this check, so an already-linked user is unaffected.
    #[error("the upstream email is not verified; refusing to link or provision an identity")]
    UnverifiedEmail,
    /// The upstream email claim is malformed (or empty) — it cannot be a local user's primary handle.
    #[error("the upstream email claim is not a usable address: {0}")]
    InvalidEmail(#[from] SignupError),
    /// The underlying identity store failed (I/O, transaction). Distinct from a policy refusal.
    #[error(transparent)]
    Store(#[from] IdentityError),
}

/// Map a VERIFIED upstream OIDC identity onto a local [`User`], linking or provisioning as needed
/// (decision D / §4.1). The `claims` MUST already have passed `qfs-oauth` ID-token verification — this
/// function trusts the `(provider, subject)` pair and the `email_verified` bit, and does NO crypto.
///
/// The rule, in order:
/// 1. `(provider, subject)` already linked → [`LinkOutcome::ExistingLink`] (returning user; the email
///    claim is not consulted — the proven subject is the anchor).
/// 2. else the email MUST be verified ([`LinkError::UnverifiedEmail`] otherwise) and well-shaped
///    ([`LinkError::InvalidEmail`] otherwise);
/// 3. a local user with that email exists → link a new account onto it
///    ([`LinkOutcome::LinkedToExistingUser`]);
/// 4. else provision a new user + link the account ([`LinkOutcome::CreatedNewUser`]).
///
/// The federated account is created with NO password hash (`provider != 'local'`), exactly the
/// "OAuth/OIDC account" shape t45 reserved.
///
/// # Errors
/// [`LinkError::UnverifiedEmail`] / [`LinkError::InvalidEmail`] on a policy/shape refusal;
/// [`LinkError::Store`] on a store failure.
pub fn link_or_create_from_oidc(
    store: &dyn IdentityStore,
    claims: &OidcClaims,
) -> Result<LinkOutcome, LinkError> {
    // 1. A returning federated user: the `(provider, subject)` row is the trust anchor (the upstream
    //    signature already proved the subject). The email claim — verified or not — is irrelevant to
    //    re-recognising someone we have already linked.
    if let Some(account) = store.find_account(&claims.provider, &claims.subject)? {
        let user = store.find_user_by_id(account.user_id)?.ok_or_else(|| {
            // A linked account whose user vanished is a corrupt store, not a normal outcome.
            IdentityError::Backend(format!(
                "account {} links to missing user {}",
                account.id, account.user_id
            ))
        })?;
        return Ok(LinkOutcome::ExistingLink(user));
    }

    // 2. No prior link → we are about to bind this federated login to a LOCAL identity by email. That
    //    is only safe for a VERIFIED email (account-takeover guard); an unverified claim fails closed.
    if !claims.email_verified {
        return Err(LinkError::UnverifiedEmail);
    }
    // The email becomes (or matches) a local user's primary handle, so it must be well-shaped. Match
    // t45's normalisation (trim + lower-case) so the lookup/insert agree with local sign-up.
    validate_email(&claims.email)?;
    let email = claims.email.trim().to_lowercase();

    // 3. An existing local user with this verified email → attach the federated account to it.
    if let Some(user) = store.find_user_by_email(&email)? {
        store.create_account(user.id, &claims.provider, &claims.subject, None)?;
        return Ok(LinkOutcome::LinkedToExistingUser(user));
    }

    // 4. Brand-new user: provision the `users` row, then link the federated `accounts` row.
    let user = store.create_user(&email)?;
    store.create_account(user.id, &claims.provider, &claims.subject, None)?;
    Ok(LinkOutcome::CreatedNewUser(user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, AccountId, SoleUser, UserId, UserStatus};
    use crate::password::PasswordHash;
    use crate::Secret;
    use std::sync::Mutex;

    /// A tiny in-memory [`IdentityStore`] so the linking RULE is tested with zero I/O (the rusqlite
    /// impl is exercised separately in `qfs-store`). Models the two uniqueness constraints the SQLite
    /// schema enforces: `users.primary_email` and `accounts(provider, subject)`.
    #[derive(Default)]
    struct MemStore {
        inner: Mutex<MemState>,
    }

    #[derive(Default)]
    struct MemState {
        users: Vec<User>,
        accounts: Vec<Account>,
    }

    impl MemStore {
        fn next_id(len: usize) -> i64 {
            i64::try_from(len).unwrap() + 1
        }
    }

    impl IdentityStore for MemStore {
        fn create_user(&self, email: &str) -> Result<User, IdentityError> {
            let mut s = self.inner.lock().unwrap();
            if s.users.iter().any(|u| u.primary_email == email) {
                return Err(IdentityError::DuplicateEmail);
            }
            let user = User {
                id: UserId(MemStore::next_id(s.users.len())),
                primary_email: email.to_string(),
                created_at: "2026-06-28T00:00:00Z".to_string(),
                status: UserStatus::Active,
            };
            s.users.push(user.clone());
            Ok(user)
        }

        fn find_user_by_email(&self, email: &str) -> Result<Option<User>, IdentityError> {
            let s = self.inner.lock().unwrap();
            Ok(s.users.iter().find(|u| u.primary_email == email).cloned())
        }

        fn find_user_by_id(&self, id: UserId) -> Result<Option<User>, IdentityError> {
            let s = self.inner.lock().unwrap();
            Ok(s.users.iter().find(|u| u.id == id).cloned())
        }

        fn create_account(
            &self,
            user_id: UserId,
            provider: &str,
            subject: &str,
            _password_hash: Option<&PasswordHash>,
        ) -> Result<Account, IdentityError> {
            let mut s = self.inner.lock().unwrap();
            if s.accounts
                .iter()
                .any(|a| a.provider == provider && a.subject == subject)
            {
                return Err(IdentityError::DuplicateAccount);
            }
            let account = Account {
                id: AccountId(MemStore::next_id(s.accounts.len())),
                user_id,
                provider: provider.to_string(),
                subject: subject.to_string(),
                created_at: "2026-06-28T00:00:00Z".to_string(),
            };
            s.accounts.push(account.clone());
            Ok(account)
        }

        fn find_account(
            &self,
            provider: &str,
            subject: &str,
        ) -> Result<Option<Account>, IdentityError> {
            let s = self.inner.lock().unwrap();
            Ok(s.accounts
                .iter()
                .find(|a| a.provider == provider && a.subject == subject)
                .cloned())
        }

        fn verify_password(
            &self,
            _provider: &str,
            _subject: &str,
            _candidate: &Secret,
        ) -> Result<bool, IdentityError> {
            Ok(false)
        }

        fn signup_local(
            &self,
            email: &str,
            _password_hash: &PasswordHash,
        ) -> Result<User, IdentityError> {
            let user = self.create_user(email)?;
            self.create_account(user.id, crate::PROVIDER_LOCAL, email, None)?;
            Ok(user)
        }

        fn sole_user(&self) -> Result<SoleUser, IdentityError> {
            let s = self.inner.lock().unwrap();
            Ok(match s.users.len() {
                0 => SoleUser::None,
                1 => SoleUser::One(s.users[0].clone()),
                _ => SoleUser::Many,
            })
        }
    }

    fn claims(verified: bool) -> OidcClaims {
        OidcClaims {
            provider: "https://idp.example".to_string(),
            subject: "upstream-subject-1".to_string(),
            email: "Alice@Example.com".to_string(),
            email_verified: verified,
        }
    }

    #[test]
    fn a_verified_first_login_provisions_a_new_user_and_links_the_account() {
        let store = MemStore::default();
        let outcome = link_or_create_from_oidc(&store, &claims(true)).unwrap();
        let user = match &outcome {
            LinkOutcome::CreatedNewUser(u) => u.clone(),
            other => panic!("expected a fresh provision, got {other:?}"),
        };
        // The email was normalised (lower-cased) into the primary handle.
        assert_eq!(user.primary_email, "alice@example.com");
        // The federated account is keyed by (provider, subject) and carries no password.
        let acct = store
            .find_account("https://idp.example", "upstream-subject-1")
            .unwrap()
            .unwrap();
        assert_eq!(acct.user_id, user.id);
    }

    #[test]
    fn a_second_login_by_the_same_subject_links_to_the_same_user() {
        let store = MemStore::default();
        let first = link_or_create_from_oidc(&store, &claims(true)).unwrap();
        let first_user = first.user().clone();

        // Same provider+subject again — even if the email claim later arrives UNVERIFIED, the proven
        // subject re-recognises the same user (the (provider, subject) anchor wins).
        let again = link_or_create_from_oidc(&store, &claims(false)).unwrap();
        match &again {
            LinkOutcome::ExistingLink(u) => assert_eq!(u.id, first_user.id),
            other => panic!("expected the existing link, got {other:?}"),
        }
        // Exactly one user + one account — no duplicate provisioning.
        let s = store.inner.lock().unwrap();
        assert_eq!(s.users.len(), 1);
        assert_eq!(s.accounts.len(), 1);
    }

    #[test]
    fn a_verified_email_links_onto_an_existing_local_user() {
        let store = MemStore::default();
        // A pre-existing LOCAL user with the same email (e.g. signed up with a password earlier).
        let local = store.create_user("alice@example.com").unwrap();
        store
            .create_account(local.id, crate::PROVIDER_LOCAL, "alice@example.com", None)
            .unwrap();

        let outcome = link_or_create_from_oidc(&store, &claims(true)).unwrap();
        match &outcome {
            LinkOutcome::LinkedToExistingUser(u) => assert_eq!(u.id, local.id),
            other => panic!("expected a link onto the existing user, got {other:?}"),
        }
        // The federated account now points at the SAME local user (one human, two sign-in faces).
        let acct = store
            .find_account("https://idp.example", "upstream-subject-1")
            .unwrap()
            .unwrap();
        assert_eq!(acct.user_id, local.id);
        // No second user was created.
        assert_eq!(store.inner.lock().unwrap().users.len(), 1);
    }

    #[test]
    fn an_unverified_email_does_not_auto_link_or_provision() {
        let store = MemStore::default();
        // A pre-existing local user owns this email; an unverified upstream claim for the SAME email
        // (but a never-seen subject) must NOT hijack it.
        let local = store.create_user("alice@example.com").unwrap();
        store
            .create_account(local.id, crate::PROVIDER_LOCAL, "alice@example.com", None)
            .unwrap();

        let err = link_or_create_from_oidc(&store, &claims(false)).unwrap_err();
        assert_eq!(err, LinkError::UnverifiedEmail);
        // Nothing was linked or created: still one user, one (local) account.
        let s = store.inner.lock().unwrap();
        assert_eq!(s.users.len(), 1);
        assert_eq!(s.accounts.len(), 1);
    }

    #[test]
    fn a_malformed_verified_email_is_rejected() {
        let store = MemStore::default();
        let mut c = claims(true);
        c.email = "not-an-email".to_string();
        let err = link_or_create_from_oidc(&store, &c).unwrap_err();
        assert!(matches!(err, LinkError::InvalidEmail(_)), "got {err:?}");
        assert!(store.inner.lock().unwrap().users.is_empty());
    }
}
