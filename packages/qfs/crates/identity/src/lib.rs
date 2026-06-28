//! `qfs-identity` — the identity domain core (roadmap **M1 / t45**): the first real notion of
//! *who a human is* in qfs.
//!
//! This crate owns the pure-ish domain model and policy; the SQLite I/O that backs it is **injected**
//! by the terminal binary (the rusqlite `IdentityStore` impl lives in `qfs-store`, which the binary
//! wires). It is a **leaf**: NO tokio, NO rusqlite, NO `qfs-lang`/`qfs-plan`/`qfs-driver`/`qfs-codec`/
//! `qfs-parser`. Its only workspace edges are [`qfs_secrets`] (for [`Secret`]) and `qfs-crypto-core`
//! (for the constant-time compare). The dep-direction guard in `crates/cmd/tests/dep_direction.rs`
//! mechanically enforces that confinement.
//!
//! ## What this is — and is NOT
//! This ticket is **AUTHENTICATION ONLY** (roadmap decision **§4.1**: *identity is not
//! authorization*). It answers "who are you" — sign up with an email + password, look yourself up —
//! and deliberately stops short of sessions (t46) and OAuth/OIDC (M2). A signed-up user can do
//! **nothing** privileged yet; that is intentional, not an omission. There is **local sign-up, no
//! session**.
//!
//! ## The identity [`AccountId`] is NOT the credential connection id
//! t44 renamed the *credential* concept to `connection` (`qfs_secrets::ConnectionId` /
//! `CredentialKey` — a stored service token like a Gmail or S3 secret). The identity [`AccountId`]
//! here is a **different, new** type: a *linked sign-in identity* for a human (a local password
//! today, an OAuth/OIDC subject later), many-to-one against a [`User`]. The two share neither name
//! nor table. Do not conflate a sign-in identity with a service credential.
//!
//! ## Secret hygiene (RFD §10)
//! The plaintext password is a [`Secret`] (redacting, zeroized on drop); it is hashed with argon2id
//! and the plaintext is dropped (zeroized) immediately after. The [`PasswordHash`] is never logged,
//! never returned by `whoami`, never serialized into an audit event. Verification is constant-time
//! via `qfs_crypto_core::constant_time_eq`.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod invite;
mod invite_store;
mod model;
mod password;
mod signup;
mod store;

pub use invite::{
    Invite, InviteId, InviteStatus, InviteToken, Membership, MembershipId, MembershipScope,
    NewInvite, Role,
};
pub use invite_store::{InviteError, InviteStore, Redemption};
pub use model::{Account, AccountId, SoleUser, User, UserId, UserStatus, PROVIDER_LOCAL};
pub use password::{hash_password, verify_password, PasswordError, PasswordHash};
pub use signup::{
    validate_email, validate_password, SignupError, MAX_PASSWORD_LEN, MIN_PASSWORD_LEN,
};
pub use store::{IdentityError, IdentityStore};

// Re-export `Secret` so every consumer — including the injected rusqlite `IdentityStore` impl in
// `qfs-store` — names the SAME redacting/zeroized password wrapper without a second edge to
// qfs-secrets just for the type.
pub use qfs_secrets::Secret;
