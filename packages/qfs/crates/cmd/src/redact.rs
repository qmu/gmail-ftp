//! [`LogScrubber`] — the **defense-in-depth** log redactor (t37, RFD §10).
//!
//! ## The primary control is `Secret<T>`, not this
//! The headline guarantee that credentials never reach a log is that the only type holding live
//! key material — `qfs_secrets::Secret` — has a redacting `Debug`/`Display` and no `Serialize`,
//! so a secret simply cannot be *formatted* into a span/event. This scrubber is the **backup**:
//! a belt-and-suspenders pass that catches a secret SHAPE that slipped into a log line through a
//! path that never went through `Secret` (a raw `&str` token a driver logged, a URL with a
//! `?sig=` query parameter, an `Authorization: Bearer …` header rendered by a third party). It is
//! NOT a license to log secrets; it is the last net under the type-system guarantee.
//!
//! ## Why it is installed at the logging init (all sinks, any call site)
//! It is wired into [`crate::init_tracing`] as the subscriber's writer wrapper, so EVERY line the
//! fmt subscriber emits — from any crate, any span, any event — passes through [`scrub`] before
//! it reaches the byte sink. Placing it at the init means a careless `tracing::info!` anywhere in
//! the workspace is scrubbed without that call site having to know about redaction.
//!
//! ## What it scans (documented, conservative, no regex/vendor dep)
//! Pure string scanning over a small set of high-signal secret shapes:
//! - **Auth-scheme tokens**: `Bearer <token>` / `Basic <token>` / `X-API-Key: <key>` → the token.
//! - **Signature query params**: `?sig=…` / `&signature=…` / `…-Signature=…` → the value.
//! - **HTTP basic-auth in a URL**: `scheme://user:pass@host` → the `user:pass` is replaced.
//!
//! It is deliberately conservative (it would rather miss an exotic shape than corrupt a benign
//! line); the real guarantee remains `Secret` never being formatted.

/// The marker every scrubbed value is replaced with (kept in sync with the secrets crate's
/// redaction token so a scrubbed line reads consistently with a `Secret`'s own `Debug`).
pub const REDACTED: &str = "***redacted***";

/// Scrub known secret SHAPES from one rendered log line, returning the sanitised string. Pure and
/// allocation-light (it returns the input unchanged — borrowed via `Cow`-free clone only when a
/// match is found is not worth the complexity here; we always return an owned `String` for a
/// uniform writer path). Defense-in-depth only — see the module docs.
#[must_use]
pub fn scrub(line: &str) -> String {
    let mut out = line.to_string();
    out = scrub_bearer(&out);
    out = scrub_sig_query(&out);
    out = scrub_basic_auth(&out);
    out
}

/// The core single-pass replacer: scan `s` (case-insensitively on `markers`) left-to-right; after
/// each marker, replace the value span (delimited by `is_delim`) with [`REDACTED`] and CONTINUE
/// scanning from AFTER the inserted marker — so a replacement can never be re-matched (the bug a
/// re-scan-from-start loop would have). Terminating by construction (the cursor only advances).
fn scrub_after_markers(s: &str, markers: &[&str], is_delim: impl Fn(char) -> bool) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    while cursor < s.len() {
        // Find the EARLIEST next marker at or after `cursor`.
        let next = markers
            .iter()
            .filter_map(|m| lower[cursor..].find(m).map(|rel| (cursor + rel, m.len())))
            .min_by_key(|(pos, _)| *pos);
        let Some((pos, mlen)) = next else {
            // No more markers: copy the rest verbatim.
            out.push_str(&s[cursor..]);
            break;
        };
        let val_start = pos + mlen;
        // Copy everything up to and including the marker.
        out.push_str(&s[cursor..val_start]);
        // The value runs to the next delimiter (or end).
        let rest = &s[val_start..];
        let end = rest.find(&is_delim).map_or(rest.len(), |e| e);
        if end > 0 {
            out.push_str(REDACTED);
        }
        // Advance the cursor PAST the value (never re-scanning the inserted marker).
        cursor = val_start + end;
    }
    out
}

/// Replace the token following a `Bearer ` / `authorization` marker. Case-insensitive on the
/// marker; the replaced span runs to the next whitespace, quote, or comma.
fn scrub_bearer(s: &str) -> String {
    // The scheme markers (`bearer `/`basic `) directly precede the token and are preferred. We do
    // NOT add a bare `authorization: ` marker because it would only redact the SCHEME word (the
    // span up to the next space) and leave the token; the scheme markers catch the token itself.
    // `x-api-key: ` precedes a raw key with no scheme word, so it is included.
    let markers = ["bearer ", "basic ", "x-api-key: ", "x-api-key="];
    scrub_after_markers(s, &markers, |c| {
        c.is_whitespace() || c == '"' || c == ',' || c == '\''
    })
}

/// Replace the value of a signature-bearing query parameter (`sig`, `signature`, or any
/// `X-…-Signature`-style key). The value runs to the next `&`, whitespace, quote, or end.
fn scrub_sig_query(s: &str) -> String {
    let keys = [
        "?sig=",
        "&sig=",
        "?signature=",
        "&signature=",
        "-signature=",
    ];
    scrub_after_markers(s, &keys, |c| {
        c == '&' || c.is_whitespace() || c == '"' || c == '\''
    })
}

/// Replace the `user:pass` userinfo in a `scheme://user:pass@host` URL (HTTP basic-auth in a
/// URL). Conservative: only fires when a `://` is followed by a `:` then an `@` before the next
/// `/`, `?`, whitespace, or quote.
fn scrub_basic_auth(s: &str) -> String {
    let Some(scheme_pos) = s.find("://") else {
        return s.to_string();
    };
    let after = scheme_pos + 3;
    let authority = &s[after..];
    // The authority ends at the first path/query/space.
    let auth_end = authority
        .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"')
        .map_or(authority.len(), |e| e);
    let authority = &authority[..auth_end];
    if let Some(at) = authority.find('@') {
        let userinfo = &authority[..at];
        if userinfo.contains(':') {
            let mut rebuilt = String::with_capacity(s.len());
            rebuilt.push_str(&s[..after]);
            rebuilt.push_str(REDACTED);
            rebuilt.push_str(&s[after + at..]);
            return rebuilt;
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_bearer_token() {
        let line = "GET /x Authorization: Bearer sk-LIVE-7f9c-PLANTED done";
        let out = scrub(line);
        assert!(!out.contains("sk-LIVE-7f9c-PLANTED"), "{out}");
        assert!(out.contains(REDACTED), "{out}");
        assert!(out.contains("done"), "trailing context preserved: {out}");
    }

    #[test]
    fn scrubs_signature_query_param() {
        let line = "fetched https://h/p?ts=1&sig=ABC123SECRET&v=2";
        let out = scrub(line);
        assert!(!out.contains("ABC123SECRET"), "{out}");
        assert!(out.contains("v=2"), "the trailing param is kept: {out}");
        assert!(out.contains("ts=1"), "the leading param is kept: {out}");
    }

    #[test]
    fn scrubs_uppercase_signature_header_style_query() {
        let line = "url=https://h/p?X-Amz-Signature=DEADBEEFsecret&z=1";
        let out = scrub(line);
        assert!(!out.contains("DEADBEEFsecret"), "{out}");
        assert!(out.contains("z=1"), "{out}");
    }

    #[test]
    fn scrubs_basic_auth_userinfo() {
        let line = "connecting to https://admin:hunter2@host/path?x=1";
        let out = scrub(line);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(!out.contains("admin:hunter2"), "{out}");
        assert!(out.contains("host/path"), "the host is preserved: {out}");
    }

    #[test]
    fn leaves_a_benign_line_unchanged() {
        let line = "INSERT /mail/outbox affected=3 ts=1700000000";
        assert_eq!(scrub(line), line);
    }

    #[test]
    fn handles_multiple_shapes_in_one_line() {
        let line = "Bearer tokABC then https://u:p@h/?sig=SIGVAL end";
        let out = scrub(line);
        assert!(!out.contains("tokABC"), "{out}");
        assert!(!out.contains("SIGVAL"), "{out}");
        assert!(!out.contains("u:p@"), "{out}");
        assert!(out.contains("end"), "{out}");
    }
}
