//! Pure `Set-Cookie` formatting and `Cookie` parsing — no HTTP framework, no I/O.
//!
//! The terminal binary builds the `Set-Cookie` header VALUE here and attaches it to an
//! `HttpResponse`; the request path reads the `Cookie` header VALUE and extracts the token here.
//! Both `Cookie` and `Set-Cookie` are in `qfs_http_core::SENSITIVE_HEADERS`, so the token they carry
//! is redacted in every log/trace line.

use crate::SessionToken;

/// The session cookie name.
///
/// **OPEN PRODUCT DECISION (flagged for the reviewer, t46 — not baked in):** the ticket leaves the
/// exact cookie name open. `qfs_session` is the least-surprising default (the example in the
/// ticket); a later ticket may prefix/host-scope it (`__Host-`) once HTTPS is mandatory.
pub const COOKIE_NAME: &str = "qfs_session";

/// Format the `Set-Cookie` header VALUE that issues `token`:
/// `qfs_session=<token>; HttpOnly; SameSite=Lax; Path=/; Max-Age=<ttl_secs>` (+ `; Secure` when
/// `secure`). `HttpOnly` keeps the token out of JS; `SameSite=Lax` is a baseline CSRF defense
/// (a state-changing browser POST will additionally need a double-submit token — out of scope here,
/// noted as the t46 seam); `Secure` is gated on an injected trusted-HTTPS flag so plain-localhost
/// dev still works.
///
/// This is the ONE place a live token is rendered onto the wire — it goes through the explicit
/// [`crate::Secret::expose_str`] door. The token is ASCII hex (valid UTF-8), so the expose never
/// fails; an unexpected non-UTF-8 token yields an empty value rather than a panic.
#[must_use]
pub fn format_set_cookie(token: &SessionToken, ttl_secs: i64, secure: bool) -> String {
    let value = token.reveal().expose_str().unwrap_or("");
    let mut out =
        format!("{COOKIE_NAME}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={ttl_secs}");
    if secure {
        out.push_str("; Secure");
    }
    out
}

/// Format the `Set-Cookie` header VALUE that CLEARS the session cookie (sign-out): an empty value
/// with `Max-Age=0` so the browser drops it immediately. Same attributes as [`format_set_cookie`]
/// so the browser scopes the deletion to the same cookie.
#[must_use]
pub fn format_clear_cookie(secure: bool) -> String {
    let mut out = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    if secure {
        out.push_str("; Secure");
    }
    out
}

/// Extract the session token from a `Cookie` request-header VALUE (e.g.
/// `theme=dark; qfs_session=abc123; lang=en`). Returns the `qfs_session` value if present, else
/// `None`. Splits on `;`, trims each pair, and matches the cookie NAME exactly (so a `not_qfs_session`
/// cookie never matches). The returned value is opaque — the caller hashes it before any lookup.
#[must_use]
pub fn parse_cookie_header(header: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=') {
            if name.trim() == COOKIE_NAME {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> SessionToken {
        SessionToken::from_entropy(&[0xde, 0xad, 0xbe, 0xef])
    }

    #[test]
    fn set_cookie_has_the_expected_attributes_and_token() {
        let c = format_set_cookie(&token(), 3600, false);
        assert!(c.starts_with("qfs_session=deadbeef;"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("Max-Age=3600"));
        // Not secure on plain localhost.
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn set_cookie_adds_secure_when_trusted_https() {
        let c = format_set_cookie(&token(), 60, true);
        assert!(c.ends_with("; Secure"), "secure flag appended: {c}");
    }

    #[test]
    fn clear_cookie_expires_immediately() {
        let c = format_clear_cookie(false);
        assert!(c.starts_with("qfs_session=;"));
        assert!(c.contains("Max-Age=0"));
    }

    #[test]
    fn parse_extracts_the_session_token_among_other_cookies() {
        assert_eq!(
            parse_cookie_header("theme=dark; qfs_session=abc123; lang=en"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_cookie_header("qfs_session=solo"),
            Some("solo".to_string())
        );
    }

    #[test]
    fn parse_returns_none_when_absent_or_a_lookalike() {
        assert_eq!(parse_cookie_header("theme=dark; lang=en"), None);
        // A look-alike name must not match the exact cookie name.
        assert_eq!(parse_cookie_header("not_qfs_session=abc"), None);
        assert_eq!(parse_cookie_header(""), None);
    }

    #[test]
    fn format_then_parse_round_trips_the_token() {
        let issued = token();
        let raw_value = issued.reveal().expose_str().unwrap().to_string();
        let set_cookie = format_set_cookie(&issued, 3600, false);
        // Simulate the browser echoing the cookie back: `Cookie: qfs_session=<value>`.
        let strip = set_cookie.split(';').next().unwrap(); // "qfs_session=deadbeef"
        let parsed = parse_cookie_header(strip).unwrap();
        assert_eq!(parsed, raw_value);
    }
}
