//! [`build_mime`] — the **pure** RFC 5322 / `multipart/mixed` message builder (RFD-0001 §6
//! recovery, the ticket's "MIME correctness" hard part).
//!
//! It produces a byte-stable message from a [`MailDraft`]: CRLF line endings, RFC 2047
//! base64-encoded `Subject` when non-ASCII, a `multipart/mixed` body with a deterministic
//! boundary when attachments are present, and per-attachment base64 parts. The whole message is
//! then base64url-encoded for the Gmail `raw` field (Gmail requires **base64url**, not standard
//! base64). No I/O, no clock, no randomness — so the output is golden-test stable.
//!
//! The base64 alphabets are implemented here (no external crate) so the encoding is fully owned
//! and deterministic, and the crate's no-vendor-leak surface stays minimal.

use crate::error::GmailError;
use crate::schema::MailDraft;

/// CRLF — the required line terminator in RFC 5322 messages.
const CRLF: &str = "\r\n";

/// The fixed multipart boundary. Deterministic (no randomness) so `build_mime` is byte-stable
/// for golden tests; it is distinctive enough not to collide with body/attachment content.
const BOUNDARY: &str = "cfs-mail-boundary-0a1b2c3d4e5f";

/// Build the RFC 5322 message bytes for `draft` (CRLF, RFC 2047 subject, `multipart/mixed`
/// when attachments are present). Returns the **raw** message bytes (not yet base64url-wrapped);
/// use [`raw_base64url`] for the Gmail `raw` field.
///
/// # Errors
/// [`GmailError::Mime`] if the draft names no recipients (`To` is empty).
pub fn build_mime(draft: &MailDraft) -> Result<Vec<u8>, GmailError> {
    if draft.to.is_empty() {
        return Err(GmailError::Mime {
            reason: "draft has no `To` recipients",
        });
    }

    let mut msg = String::new();
    msg.push_str(&format!("To: {}{CRLF}", draft.to.join(", ")));
    if !draft.cc.is_empty() {
        msg.push_str(&format!("Cc: {}{CRLF}", draft.cc.join(", ")));
    }
    msg.push_str(&format!(
        "Subject: {}{CRLF}",
        encode_subject(&draft.subject)
    ));
    msg.push_str(&format!("MIME-Version: 1.0{CRLF}"));

    if draft.attachments.is_empty() {
        // Simple text message.
        msg.push_str(&format!(
            "Content-Type: text/plain; charset=\"UTF-8\"{CRLF}"
        ));
        msg.push_str(CRLF);
        msg.push_str(&normalize_crlf(&draft.body));
    } else {
        // multipart/mixed: a text part, then one part per attachment.
        msg.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{BOUNDARY}\"{CRLF}"
        ));
        msg.push_str(CRLF);

        // Text body part.
        msg.push_str(&format!("--{BOUNDARY}{CRLF}"));
        msg.push_str(&format!(
            "Content-Type: text/plain; charset=\"UTF-8\"{CRLF}"
        ));
        msg.push_str(CRLF);
        msg.push_str(&normalize_crlf(&draft.body));
        msg.push_str(CRLF);

        // Attachment parts.
        for att in &draft.attachments {
            msg.push_str(&format!("--{BOUNDARY}{CRLF}"));
            msg.push_str(&format!("Content-Type: {}{CRLF}", att.mime));
            msg.push_str(&format!("Content-Transfer-Encoding: base64{CRLF}"));
            msg.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{}\"{CRLF}",
                att.filename.replace('"', "")
            ));
            msg.push_str(CRLF);
            msg.push_str(&wrap76(&base64_standard(&att.bytes)));
            msg.push_str(CRLF);
        }

        // Closing boundary.
        msg.push_str(&format!("--{BOUNDARY}--{CRLF}"));
    }

    Ok(msg.into_bytes())
}

/// Build the base64url-encoded `raw` value for the Gmail `messages.send`/`drafts` API from a
/// draft — `build_mime` then base64url (the Gmail `raw` field requires base64url, no padding
/// stripping needed; Gmail accepts standard base64url with padding).
///
/// # Errors
/// [`GmailError::Mime`] propagated from [`build_mime`].
pub fn raw_base64url(draft: &MailDraft) -> Result<String, GmailError> {
    let bytes = build_mime(draft)?;
    Ok(base64_url(&bytes))
}

/// Encode a `Subject` value: ASCII passes through verbatim; a non-ASCII subject is RFC 2047
/// base64-encoded (`=?UTF-8?B?...?=`) so a unicode subject is wire-safe and byte-stable.
fn encode_subject(subject: &str) -> String {
    if subject.is_ascii() {
        subject.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", base64_standard(subject.as_bytes()))
    }
}

/// Normalize any bare `\n` in the body to CRLF (RFC 5322 line endings), leaving existing CRLFs
/// intact.
fn normalize_crlf(body: &str) -> String {
    body.replace("\r\n", "\n").replace('\n', CRLF)
}

/// Wrap a base64 string at 76 characters per line (RFC 2045 line-length limit) with CRLF.
fn wrap76(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 76 * 2);
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + 76).min(bytes.len());
        // Safe: base64 output is ASCII, so a 76-byte slice is a valid str boundary.
        out.push_str(&s[i..end]);
        if end < bytes.len() {
            out.push_str(CRLF);
        }
        i = end;
    }
    out
}

/// The standard base64 alphabet (RFC 4648 §4).
const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// The URL-safe base64 alphabet (RFC 4648 §5) — the Gmail `raw` field alphabet.
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Standard base64-encode bytes (with `=` padding) — for MIME part bodies + RFC 2047 subjects.
fn base64_standard(input: &[u8]) -> String {
    base64_with(input, STD)
}

/// URL-safe base64-encode bytes (with `=` padding) — for the Gmail `raw` message field.
fn base64_url(input: &[u8]) -> String {
    base64_with(input, URL)
}

/// The shared base64 core, parameterised by alphabet. Deterministic, panic-free.
fn base64_with(input: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((triple >> 18) & 0x3F) as usize] as char);
        out.push(alphabet[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
