//! `qfs-session` — the session domain core (roadmap **M1 / t46**): server-side sessions for the
//! local web / dashboard face, built ON the t45 identity store.
//!
//! A **session** binds an HTTP request to a [`qfs_identity::UserId`] — the *authentication state*
//! that a successful local password verify (t45) produces. This crate owns the pure-ish domain:
//! the [`Session`]/[`SessionId`] model + expiry logic, the opaque high-entropy [`SessionToken`]
//! (wrapped in the redacting [`Secret`], **hashed at rest**), the consumer-side [`SessionStore`]
//! trait (`create`/`lookup`/`rotate`/`revoke`), and pure `Set-Cookie`/`Cookie` formatting+parsing.
//! The SQLite I/O that backs the store is **injected** by the terminal binary (the rusqlite
//! `SqliteSessionStore` lives in `qfs-store`); token entropy is injected too (see below).
//!
//! ## What this is — and is NOT (decision §4.1)
//! This is **AUTHENTICATION STATE ONLY**. A session proves *who you are*; it grants *nothing* by
//! itself this milestone. Authorization (policy / OAuth) is **M2** — until it lands, an attached
//! session is **inert**: no data path may silently trust it. Refusing unauthenticated requests is
//! deferred too (t50 for MCP, t51 for the SPA); this crate only *issues* and *validates* sessions.
//!
//! ## Token hygiene (security-first, RFD §10)
//! The opaque token is generated from a CSPRNG in the **binary leaf** (OS entropy) and handed to
//! [`SessionToken::from_entropy`] — keeping THIS core deterministic and testable (no `rand`/
//! `getrandom` edge here). The DB stores **only** `sha256_hex(token)` ([`SessionToken::hash`]); the
//! plaintext token lives only in the [`Secret`]-wrapped cookie value, returned to the caller exactly
//! once at issue. A presented token is validated by hashing it and looking the hash up (a
//! constant-time compare backs the verification — see the store impl). `Cookie`/`Set-Cookie` are in
//! `qfs_http_core::SENSITIVE_HEADERS`, so the token is redacted in every log/trace/audit line.
//!
//! ## A leaf
//! Its only workspace edges are [`qfs_identity`] (the [`qfs_identity::UserId`] a session belongs
//! to), [`qfs_secrets`] (the redacting [`Secret`]), and `qfs-crypto-core` (the at-rest hash +
//! constant-time compare). NO tokio, NO rusqlite, NO `qfs-lang`/`qfs-plan`/`qfs-driver`/`qfs-codec`/
//! `qfs-parser`. The dep-direction guard in `crates/cmd/tests/dep_direction.rs` mechanically
//! enforces that confinement.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod auth;
mod cookie;
mod model;
mod store;
mod token;

pub use auth::authenticate;
pub use cookie::{format_clear_cookie, format_set_cookie, parse_cookie_header, COOKIE_NAME};
pub use model::{Session, SessionId};
pub use store::{SessionError, SessionStore};
pub use token::{token_hash, SessionToken};

// Re-export the two cross-crate types a session is expressed in terms of, so every consumer —
// including the injected rusqlite `SessionStore` impl in `qfs-store` and the binary's composition
// root — names the SAME `UserId` and `Secret` without a second edge just for the type.
pub use qfs_identity::UserId;
pub use qfs_secrets::Secret;

/// The default session lifetime in seconds (24h).
///
/// **OPEN PRODUCT DECISION (flagged for the reviewer, t46 — not baked in):** the ticket leaves the
/// exact TTLs open (a short *idle* TTL vs. an absolute *max* lifetime). This single absolute TTL is
/// the least-surprising default for the milestone; a later ticket may split idle-vs-absolute and
/// thread a refresh. Callers pass an explicit `ttl_secs` to [`SessionStore::create`]/
/// [`SessionStore::rotate`], so this constant is only the binary's chosen default.
pub const DEFAULT_SESSION_TTL_SECS: i64 = 24 * 60 * 60;
