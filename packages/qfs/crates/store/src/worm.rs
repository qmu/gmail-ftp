//! The **pure** WORM / transparency-log seam for audit-chain seals (roadmap **decision V** / §4.6,
//! ticket **t78**). qfs periodically *seals* the audit chain HEAD (t76) — a signed checkpoint
//! ([`qfs_oauth::AuditSeal`], minted binary-side) — and hands that seal OUT to an **append-only
//! witness outside the server**: an S3 Object Lock bucket, a transparency log, or a signed off-box
//! anchor. A hash chain alone can be re-forged wholesale by whoever controls the audit store; a seal
//! that has already landed in an *immutable, external* witness means a compromised server cannot
//! rewrite history below a sealed `seq` without contradicting an anchor it can no longer change.
//!
//! ## What this module is (the PURE half)
//! - [`SealRecord`] — the metadata-only line a seal becomes when it is handed to a witness: the
//!   sealed head's `(seq, content_hash, prev_hash)`, the `issued_at`, and the OPAQUE seal token (the
//!   compact ES256 JWS the AS signed; this crate never interprets it — `qfs-store` has no `qfs-oauth`
//!   edge, exactly like the `OauthKeyStore` trades only in opaque bytes).
//! - [`WormSink`] — the append-only emit seam every witness adapter implements. Object-safe so the
//!   binary selects the active witness at run time.
//! - [`WormError`] / [`WormKind`] — the secret-free failure + the witness discriminator.
//!
//! The IMPURE half — the concrete local append-only **file** witness, the parked external
//! (S3 Object Lock / transparency-log) seam, and the seal *trigger* that reads the head and signs it
//! — lives binary-side (`crates/qfs/src/worm.rs`), because only the terminal binary opens a real
//! path / socket, holds the AS key material, and reads the System DB (decision F/V), exactly as the
//! telemetry sinks and the audit chain-head I/O do.
//!
//! ## Emit, don't store (decision V)
//! The seal goes OUT; qfs keeps only the chain HEAD (t76) to continue the chain. **Where the witness
//! lives and how long it retains seals is the consumer's / platform's concern** (qfs Cloud on the
//! managed tier; the operator's own bucket self-hosted) — qfs produces the seals; it does not run the
//! WORM store.
//!
//! ## Metadata only (RFD §10)
//! A [`SealRecord`] carries ONLY chain-position metadata + the public signature token. There is
//! structurally NOWHERE to put a secret or a row's payload — the same boundary the audit event and
//! the telemetry record enforce. The line is safe to append to a public transparency log.

use std::fmt::Write as _;

/// Which append-only witness a [`WormSink`] writes to (for diagnostics / `/sys` reporting). A
/// **closed set**: a new witness transport is a new variant here, never a side-channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WormKind {
    /// The local append-only **file** witness (the zero-dependency default — point it at a path on a
    /// disk the server cannot rewrite, or sync it off-box). The concrete sink lives binary-side.
    LocalFile,
    /// An **external** write-once witness — S3 Object Lock first, a transparency log / signed
    /// off-box anchor behind the same seam. A present-but-parked exporter seam offline (no vetted
    /// transparency-log client crate in the build cache; t78 does not hand-roll RFC 6962 / a vendor
    /// protocol), wired binary-side when a vetted client lands.
    External,
}

impl WormKind {
    /// The canonical lower-case token naming this witness.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalFile => "local-file",
            Self::External => "external",
        }
    }
}

/// One emitted seal as it is handed to a witness — METADATA ONLY: the sealed head's `seq` /
/// `content_hash` / `prev_hash` (the t76 [`crate::audit::ChainHead`]), the `issued_at` wall clock,
/// and the OPAQUE `seal` token (the compact ES256 JWS the AS signed over exactly those head fields).
/// This crate never parses `seal` — it carries it verbatim to the witness, exactly as the
/// `OauthKeyStore` trades only in opaque key bytes (so `qfs-store` gains no `qfs-oauth` edge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealRecord {
    /// The sealed head's sequence number — the chain reached exactly this many events.
    pub seq: u64,
    /// The sealed head's content hash.
    pub content_hash: String,
    /// The sealed head's predecessor link (with `content_hash`, this recomputes the head's `hash`).
    pub prev_hash: String,
    /// When the head was sealed (RFC3339 UTC).
    pub issued_at: String,
    /// The opaque seal token — the compact ES256 JWS (`header.payload.signature`) the AS signed over
    /// the sealed head. Verified consumer-side with the published JWKS (`qfs_oauth::verify_seal`).
    pub seal: String,
}

impl SealRecord {
    /// Render this record as ONE canonical JSON Lines (JSONL) line — a single self-describing object,
    /// NO trailing newline (the sink appends the line terminator). The encoding is hand-rolled
    /// (qfs-store carries no `serde_json`, mirroring [`crate::telemetry`]) but RFC-8259-correct for
    /// the field set: strings are escaped, the field order is fixed (deterministic output). The line
    /// carries ONLY the record's labelled metadata + the public signature — there is nowhere to
    /// smuggle a secret.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let mut s = String::new();
        s.push('{');
        write_kv_num(&mut s, "seq", self.seq);
        s.push(',');
        write_kv_str(&mut s, "content_hash", &self.content_hash);
        s.push(',');
        write_kv_str(&mut s, "prev_hash", &self.prev_hash);
        s.push(',');
        write_kv_str(&mut s, "issued_at", &self.issued_at);
        s.push(',');
        write_kv_str(&mut s, "seal", &self.seal);
        s.push('}');
        s
    }
}

/// A witness failure — secret-free (it never renders a path's contents, only a stable reason). The
/// concrete binary-side sinks map their I/O / transport errors onto this so the seam stays
/// vendor-free.
#[derive(Debug, thiserror::Error)]
pub enum WormError {
    /// The witness's transport failed (file append / exporter). Carries a secret-free reason only.
    #[error("worm witness append failed: {0}")]
    Append(String),
}

/// The append-only emit seam every witness adapter implements: take one [`SealRecord`] and deliver it
/// to the configured write-once store. Object-safe (`dyn WormSink`) so the binary can select the
/// active witness at run time from config.
///
/// Unlike telemetry emission (best-effort, never breaks the operation), a seal is a deliberate,
/// externally-triggered checkpoint: the caller (the trigger unit) DOES surface an `Err` so the
/// operator / cron job learns the seal did not land — sealing is the tamper-evidence guarantee, not
/// an observation of an unrelated operation.
pub trait WormSink: Send + Sync {
    /// Append one seal record to the witness.
    ///
    /// # Errors
    /// Returns [`WormError`] if the underlying transport failed.
    fn append(&self, record: &SealRecord) -> Result<(), WormError>;

    /// Which witness this is (for diagnostics / `/sys` reporting).
    fn kind(&self) -> WormKind;
}

// --- JSONL field writers (hand-rolled; qfs-store carries no serde_json — mirrors `telemetry`) ------

/// Write a `"key":"value"` pair with the string value JSON-escaped.
fn write_kv_str(out: &mut String, key: &str, val: &str) {
    write_json_string(out, key);
    out.push(':');
    write_json_string(out, val);
}

/// Write a `"key":<u64>` pair (a sequence number — always a finite non-negative integer).
fn write_kv_num(out: &mut String, key: &str, val: u64) {
    write_json_string(out, key);
    out.push(':');
    let _ = write!(out, "{val}");
}

/// Append `s` as a quoted, RFC-8259-escaped JSON string (escapes the two structural characters and
/// the C0 control range), so a value can never break the one-line-per-record JSONL framing.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn record() -> SealRecord {
        SealRecord {
            seq: 12,
            content_hash: "a".repeat(64),
            prev_hash: "b".repeat(64),
            issued_at: "2026-06-28T00:00:00Z".to_string(),
            seal: "hdr.payload.sig".to_string(),
        }
    }

    #[test]
    fn seal_record_renders_one_metadata_only_jsonl_line() {
        let line = record().to_jsonl();
        assert!(line.starts_with('{') && line.ends_with('}'));
        assert!(!line.contains('\n'), "a JSONL record is exactly one line");
        assert!(line.contains("\"seq\":12"));
        assert!(line.contains("\"content_hash\":\""));
        assert!(line.contains("\"seal\":\"hdr.payload.sig\""));
    }

    #[test]
    fn no_secret_can_ride_a_seal_record_line() {
        // The record is built ONLY from labelled metadata + the public signature token — there is no
        // field for a secret, so a would-be token never appears in the line.
        let mut r = record();
        r.issued_at = "2026-06-28T00:00:00Z".to_string();
        assert!(!r.to_jsonl().contains("super-secret-token"));
    }

    #[test]
    fn json_strings_are_escaped_so_framing_cannot_break() {
        // A pathological seal token carrying a quote + newline must NOT break the one-line framing.
        let mut r = record();
        r.seal = "od\"d\ntoken".to_string();
        let line = r.to_jsonl();
        assert!(!line.contains('\n'), "the embedded newline must be escaped");
        assert!(line.contains("\\\"d"));
        assert!(line.contains("\\n"));
    }

    #[test]
    fn worm_kind_tokens_are_stable() {
        assert_eq!(WormKind::LocalFile.as_str(), "local-file");
        assert_eq!(WormKind::External.as_str(), "external");
    }
}
