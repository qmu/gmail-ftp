//! The [`Secret`] type — the **only** type in cfs that holds live key material
//! (RFD-0001 §10, "encrypted credential store, never logged").
//!
//! Redaction is the headline invariant of this crate. `Secret` is built so that the
//! *value* cannot escape by accident: it does **not** derive `Clone`, does **not**
//! implement `serde::Serialize`/`Deserialize`, and its [`fmt::Debug`]/[`fmt::Display`]
//! write a fixed `Secret(***redacted***)` token that contains no byte of the wrapped
//! material. The backing store is [`Zeroizing`], so the bytes are wiped from memory on
//! drop. The lone door to the value is the explicit, grep-able [`Secret::expose`].
//!
//! Why this matters: cfs holds tokens for Gmail, Drive, S3/R2, D1, GitHub, Slack, AWS
//! and Cloudflare at once — a large blast radius. A secret that lands in a log line, an
//! audit record, an error message, or a `{:?}` dump is a credential leak. By making the
//! *redacting* behaviour the only behaviour `Secret` exposes through the standard
//! formatting/serialization traits, a leak requires an explicit `.expose()` at the call
//! site, which CI can grep for.

use core::fmt;

use zeroize::Zeroizing;

/// The single redaction token rendered by every `Debug`/`Display` of a [`Secret`].
/// Pinned as a constant so tests can assert the exact, value-free output.
pub const REDACTED: &str = "***redacted***";

/// Opaque secret bytes: a token, API key, OAuth refresh token, or passphrase.
///
/// `Secret` is deliberately *minimal* in the traits it implements:
/// - **No `Clone`** — a secret is moved, never silently duplicated.
/// - **No `Serialize`/`Deserialize`** — a secret can never be written into JSON, an
///   audit record, or a config file by the normal serde path.
/// - **`Debug`/`Display` are redacting** — they emit [`REDACTED`], never the bytes.
/// - **`Zeroizing` backing** — the bytes are zeroed on drop.
///
/// The only way to read the material is [`Secret::expose`], which is intentionally
/// verbose and easy to grep for in review / CI.
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    /// Wrap owned bytes as a secret. Takes ownership so no plaintext copy lingers in
    /// the caller (move the source `Vec` in; do not clone it).
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Wrap an owned `String` as a secret (the common token case). The `String`'s heap
    /// buffer is consumed into the zeroizing store.
    #[must_use]
    pub fn from_string(value: String) -> Self {
        Self::new(value.into_bytes())
    }

    /// The **only** accessor for the wrapped bytes. Named `expose` (not `as_bytes` /
    /// `get`) so every read of live key material is explicit and grep-able — the CI
    /// guard greps for `.expose(` near `format!`/`tracing` to reject leaks.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The wrapped bytes interpreted as UTF-8, if valid — the token/string case. Returns
    /// `None` for non-UTF-8 material (e.g. a binary key) rather than lossily decoding,
    /// so a caller that needs a `&str` token cannot accidentally mangle a binary secret.
    #[must_use]
    pub fn expose_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.0).ok()
    }

    /// Length of the wrapped material in bytes. Safe to expose: a length is metadata,
    /// not key material, and lets callers reject an empty credential without revealing
    /// the value.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the wrapped material is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Redacting `Debug`: emits a fixed token, **never** the bytes. This is the trait the
/// `{:?}` formatter, `tracing`'s structured fields, and `#[derive(Debug)]` on enclosing
/// types all route through — so wrapping a secret in any of them stays redacted.
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({REDACTED})")
    }
}

/// Redacting `Display`: emits the bare token. A secret formatted with `{}` (e.g.
/// accidentally interpolated into an error message) shows only the redaction marker.
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Construct a `Secret` from a `&str` for ergonomics in callers/tests. Allocates an
/// owned copy (the `&str`'s own backing is the caller's concern); the copy is zeroized
/// on drop.
impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::from_string(value.to_owned())
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::from_string(value)
    }
}

impl From<Vec<u8>> for Secret {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A planted secret value that would be unmistakable if it ever leaked into output.
    const PLANTED: &str = "SUPER-SECRET-TOKEN-aabbccddeeff-PLANTED";

    /// Redaction is the headline invariant: neither `Debug` nor `Display` of a `Secret`
    /// may contain any byte of the wrapped material. We assert the exact redaction token
    /// AND that the planted value is absent from both renderings.
    #[test]
    fn debug_and_display_never_reveal_the_value() {
        let s = Secret::from(PLANTED);

        let dbg = format!("{s:?}");
        let disp = format!("{s}");

        assert_eq!(dbg, "Secret(***redacted***)");
        assert_eq!(disp, "***redacted***");
        assert!(
            !dbg.contains(PLANTED),
            "Debug leaked the secret value: {dbg}"
        );
        assert!(
            !disp.contains(PLANTED),
            "Display leaked the secret value: {disp}"
        );
        // Even a fragment of the value must not appear.
        assert!(!dbg.contains("aabbccddeeff"));
        assert!(!disp.contains("aabbccddeeff"));
    }

    /// A `Secret` nested inside a `#[derive(Debug)]` struct stays redacted — the derive
    /// routes the field through `Secret`'s `Debug`, so the enclosing dump is safe too.
    #[test]
    fn nested_in_derived_debug_stays_redacted() {
        // Fields are read only through the derived Debug (which clippy's dead-code pass
        // intentionally ignores), so silence the false-positive dead_code lint.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            label: &'static str,
            token: Secret,
        }
        let h = Holder {
            label: "github",
            token: Secret::from(PLANTED),
        };
        let dump = format!("{h:?}");
        assert!(dump.contains("github"), "non-secret field should render");
        assert!(
            !dump.contains(PLANTED),
            "nested secret leaked in derived Debug: {dump}"
        );
        assert!(dump.contains("***redacted***"));
    }

    /// The value is reachable ONLY through the explicit `expose`/`expose_str` doors —
    /// and what they return is exactly the wrapped material (round-trip).
    #[test]
    fn expose_is_the_only_door_and_round_trips() {
        let s = Secret::from(PLANTED);
        assert_eq!(s.expose(), PLANTED.as_bytes());
        assert_eq!(s.expose_str(), Some(PLANTED));
        assert_eq!(s.len(), PLANTED.len());
        assert!(!s.is_empty());

        let empty = Secret::new(Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    /// Non-UTF-8 material yields `None` from `expose_str` (no lossy decode) but the raw
    /// bytes are still recoverable via `expose`.
    #[test]
    fn non_utf8_secret_exposes_bytes_not_str() {
        let raw = vec![0xff, 0x00, 0xfe, 0x80];
        let s = Secret::new(raw.clone());
        assert_eq!(s.expose(), raw.as_slice());
        assert_eq!(s.expose_str(), None);
    }
}
