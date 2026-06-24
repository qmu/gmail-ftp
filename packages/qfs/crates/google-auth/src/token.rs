//! Owned token/account DTOs (RFD-0001 §9 — owned DTOs only, no vendor `Token`/`Userinfo`
//! type ever leaks out) plus the injectable [`Clock`] used for deterministic expiry.
//!
//! ## Secret discipline (RFD §10)
//! Both the access-token value and the refresh-token value are [`qfs_secrets::Secret`]: they
//! have no `Clone`, no `Serialize`, a redacting `Debug`/`Display`, and are zeroized on drop.
//! The live bytes are reachable only via `Secret::expose_str` at request-build time. The
//! `Debug` of [`AccessToken`] and [`GoogleAccount`] is therefore safe to log — it shows the
//! redaction marker and the (non-secret) expiry/email metadata only.

use std::time::Duration;

use qfs_secrets::Secret;

/// The default clock skew subtracted from an access token's lifetime so a token is treated as
/// expired slightly *before* its real expiry — avoiding a request that races the boundary and
/// comes back 401. Sixty seconds is the conventional OAuth refresh skew.
pub const DEFAULT_EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// A monotonic clock seam so token-expiry logic is testable without sleeping. The production
/// [`SystemClock`] reads `Instant::now()`; tests inject a [`ManualClock`] they can advance.
///
/// Returns a `u64` of monotonic nanoseconds rather than `Instant` so the value is trivially
/// comparable and a fake clock can synthesize one without an `Instant` source.
pub trait Clock: Send + Sync {
    /// The current monotonic time in nanoseconds. Only *differences* are meaningful; the
    /// origin is unspecified.
    fn now_nanos(&self) -> u128;
}

/// The production monotonic clock — `std::time::Instant` measured from a fixed process-start
/// origin captured on first construction.
pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    /// Build a system clock anchored at "now".
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_nanos(&self) -> u128 {
        self.origin.elapsed().as_nanos()
    }
}

/// A test/manual clock whose time only advances when the test tells it to — so "token expired"
/// is a deterministic state, not a `sleep`.
pub struct ManualClock {
    now: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    /// A manual clock starting at zero nanoseconds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Advance the clock by `d`.
    pub fn advance(&self, d: Duration) {
        let add = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.now.fetch_add(add, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now_nanos(&self) -> u128 {
        u128::from(self.now.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// A live access token: the bearer value plus the monotonic deadline (in clock nanoseconds)
/// at which it must be refreshed. The value is a [`Secret`] — never logged, never serialized.
///
/// `expires_at_nanos` is expressed on the same monotonic timeline as [`Clock::now_nanos`], so
/// [`AccessToken::is_expired`] is a pure comparison with no wall-clock dependency.
pub struct AccessToken {
    value: Secret,
    expires_at_nanos: u128,
}

impl AccessToken {
    /// Construct an access token expiring at `expires_at_nanos` on the clock's timeline.
    #[must_use]
    pub fn new(value: Secret, expires_at_nanos: u128) -> Self {
        Self {
            value,
            expires_at_nanos,
        }
    }

    /// Build from a `now` reading + a `lifetime` (the token endpoint's `expires_in`) minus a
    /// `skew`, so the token is refreshed slightly early. Saturates: a lifetime at/below the
    /// skew yields an already-expired token (forcing an immediate refresh).
    #[must_use]
    pub fn from_lifetime(
        value: Secret,
        now_nanos: u128,
        lifetime: Duration,
        skew: Duration,
    ) -> Self {
        let usable = lifetime.saturating_sub(skew);
        let expires_at_nanos = now_nanos.saturating_add(usable.as_nanos());
        Self {
            value,
            expires_at_nanos,
        }
    }

    /// The bearer value, for injection into an `Authorization: Bearer <token>` header at
    /// request-build time. The explicit `expose`-style accessor is the single, grep-able door
    /// to the live material (it returns `None` for non-UTF-8, which a real token never is).
    #[must_use]
    pub fn bearer(&self) -> Option<&str> {
        self.value.expose_str()
    }

    /// Borrow the underlying [`Secret`] (e.g. to re-store it). Never logged.
    #[must_use]
    pub fn secret(&self) -> &Secret {
        &self.value
    }

    /// Whether the token is at/past its (skew-adjusted) expiry on `clock`'s timeline.
    #[must_use]
    pub fn is_expired(&self, clock: &dyn Clock) -> bool {
        clock.now_nanos() >= self.expires_at_nanos
    }
}

/// A redacting `Debug`: the token value is shown only as the redaction marker; the expiry is
/// non-secret metadata. Safe to drop into a log line.
impl core::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AccessToken")
            .field("value", &self.value)
            .field("expires_at_nanos", &self.expires_at_nanos)
            .finish()
    }
}

/// An authorized Google account: the profile **email** (the account key — a low-sensitivity
/// identifier, safe to log) and the long-lived **refresh token** as a [`Secret`]. This is the
/// owned DTO `authorize` returns; the vendor `Userinfo` type never escapes the crate.
pub struct GoogleAccount {
    /// The profile email — the multi-account key (`google:<email>:refresh_token`).
    pub email: String,
    /// The long-lived refresh token. A [`Secret`]; persisted via [`qfs_secrets::Secrets`].
    pub refresh_token: Secret,
}

impl GoogleAccount {
    /// Construct an account from its email + refresh token.
    #[must_use]
    pub fn new(email: impl Into<String>, refresh_token: Secret) -> Self {
        Self {
            email: email.into(),
            refresh_token,
        }
    }
}

/// Redacting `Debug`: email (non-secret) + the redacted refresh token.
impl core::fmt::Debug for GoogleAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoogleAccount")
            .field("email", &self.email)
            .field("refresh_token", &self.refresh_token)
            .finish()
    }
}
