//! The hash-chained audit **event** model + its content/chain hashing and verification
//! (roadmap **decision V** / §4.6, ticket **t76**). This module is the PURE half: the event
//! record, its stable canonical serialization, the content + chain hashes, and the recompute/
//! verify helper. The IMPURE half — reading/writing the durable chain head + the bounded
//! live-tail in the System DB — lives binary-side (`crates/qfs/src/audit.rs`), because only the
//! terminal binary opens a real DB path (decision F).
//!
//! ## What qfs records (and what it deliberately does NOT)
//!
//! Each event is **metadata only** — `actor, connection, verb, path, committed, ts` — the same
//! boundary `describe` enforces (§3.2/§4.6): never a secret, never a credential, never a row's
//! payload. An [`AuditEvent`] is therefore safe to render in a preview, a log line, or the
//! `/sys/audit` live view.
//!
//! ## The chain (tamper-evidence)
//!
//! Events form a hash chain so any **edit**, **reorder**, or **deletion** at the destination is
//! detectable by recomputation (§4.6). Two digests are involved:
//!
//! - `content_hash = sha256_hex(content)` — a stable commitment to THIS event's fields alone
//!   (independent of position in the chain). Stable across runs: identical fields ⇒ identical
//!   `content_hash`.
//! - `hash = sha256_hex(content_hash ‖ prev_hash)` — the **chained** hash that threads each event
//!   onto its predecessor. The next event's `prev_hash` is this event's `hash`, so the chain is a
//!   Merkle-style spine: changing any earlier event (or dropping/reordering one) makes every later
//!   `hash` fail to recompute.
//!
//!   NOTE (a recorded design choice): t76's text writes the chained hash as
//!   `sha256_hex(content + prev_hash)`. We chain over the event's **content_hash** rather than the
//!   raw content bytes so the **durable head** — `(seq, content_hash, prev_hash)`, exactly the three
//!   columns t76 names — is *self-sufficient*: `head.hash()` is recomputable from the stored row
//!   alone (no need to retain the raw content), which is what lets qfs persist ONLY the head and
//!   still continue the chain across restarts (decision V: emit, don't retain).
//!
//! ## Emit, don't retain (decision V)
//!
//! qfs EMITS the stream; it does not store the whole log. The only durable audit state is the
//! chain HEAD (to continue the chain) plus a BOUNDED live-tail buffer (the recent events the
//! `/sys/audit` view reads). Retention/period and external sealing are the consumer's concern
//! (t77 sinks, t78 WORM/transparency log).

use qfs_crypto_core::sha256_hex;

/// Domain-separation tag mixed into every event's canonical content, so an audit content hash can
/// never collide with a hash computed for another purpose (migration checksums, run-ids, …) over
/// the same bytes. Bumping the suffix is how a future canonical-form change stays distinguishable.
const DOMAIN_TAG: &[u8] = b"qfs.audit.event.v1";

/// The genesis predecessor hash — the `prev_hash` of the FIRST event in a fresh chain (before any
/// head exists). 64 hex zeros: a well-known, content-free anchor so the first link is still a real
/// `sha256_hex(content_hash ‖ prev_hash)` rather than a special case.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// One audit event's canonical, **secret-free** content fields (§4.6). Metadata only: the acting
/// principal, the connection it acted through (t44), the write verb, the VFS path, whether the
/// effect actually committed, and the wall-clock timestamp. NEVER a secret or row payload — the
/// same boundary `describe` enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// The acting principal — a label, never a credential (`"cli"` for a one-shot `qfs run
    /// --commit`; a request-derived identity once multi-user auth lands).
    pub actor: String,
    /// The connection the effect routed through (t44): the selected `<driver>/<name>` credential
    /// handle's NAME, never its secret material.
    pub connection: String,
    /// The write verb label (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`/`CALL`/…) — the effect's kind.
    pub verb: String,
    /// The VFS path the effect targeted (e.g. `/local/notes.txt`, `/github/owner/repo/issues`).
    /// An owned, opaque path string — carries no secret (previews log it).
    pub path: String,
    /// Whether the effect actually committed (`true`), or was an *attempted* irreversible effect
    /// that did not complete (`false`) — both are recorded so the stream is the one funnel.
    pub committed: bool,
    /// The event timestamp (RFC3339 UTC). Part of the content, so two otherwise-identical effects
    /// at different times hash differently.
    pub ts: String,
}

impl AuditEvent {
    /// The **stable canonical serialization** of this event's content — the exact bytes
    /// [`content_hash`](Self::content_hash) digests.
    ///
    /// Encoding: a domain tag followed by every field in a FIXED order, each length-prefixed with
    /// its byte length as a big-endian `u64`. Length-prefixing makes the encoding *injective* — no
    /// two distinct field tuples produce the same bytes — so a value can never smuggle a field
    /// boundary (e.g. a `path` containing a separator cannot impersonate a different `(path, ts)`
    /// pair). Deterministic and platform-independent: the same fields always yield the same bytes.
    #[must_use]
    pub fn content(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_field(&mut buf, DOMAIN_TAG);
        write_field(&mut buf, self.actor.as_bytes());
        write_field(&mut buf, self.connection.as_bytes());
        write_field(&mut buf, self.verb.as_bytes());
        write_field(&mut buf, self.path.as_bytes());
        write_field(&mut buf, &[u8::from(self.committed)]);
        write_field(&mut buf, self.ts.as_bytes());
        buf
    }

    /// `content_hash = sha256_hex(content)` — a stable commitment to this event's fields alone,
    /// independent of its position in the chain. Identical fields always yield an identical hash.
    #[must_use]
    pub fn content_hash(&self) -> String {
        sha256_hex(&self.content())
    }

    /// Chain this event onto `prev_hash` at sequence `seq`, producing the recorded
    /// [`ChainedEvent`] (its content hash, the predecessor link, and the chained `hash`).
    #[must_use]
    pub fn chain(self, seq: u64, prev_hash: impl Into<String>) -> ChainedEvent {
        let prev_hash = prev_hash.into();
        let content_hash = self.content_hash();
        let hash = link_hash(&content_hash, &prev_hash);
        ChainedEvent {
            seq,
            event: self,
            content_hash,
            prev_hash,
            hash,
        }
    }
}

/// The **chained** hash linking an event (by its `content_hash`) onto its predecessor (`prev_hash`):
/// `sha256_hex(content_hash ‖ prev_hash)`. The next event's `prev_hash` is this value, so the chain
/// is tamper-evident end to end. Free function so the binary-side head logic can recompute a head's
/// own hash from the stored `(content_hash, prev_hash)` without rebuilding the event.
#[must_use]
pub fn link_hash(content_hash: &str, prev_hash: &str) -> String {
    let mut buf = Vec::with_capacity(content_hash.len() + prev_hash.len());
    buf.extend_from_slice(content_hash.as_bytes());
    buf.extend_from_slice(prev_hash.as_bytes());
    sha256_hex(&buf)
}

/// Append a length-prefixed field to `buf`: an 8-byte big-endian length, then the bytes. The
/// length prefix is what makes the whole content serialization injective (see [`AuditEvent::content`]).
fn write_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// One recorded event in the chain: the event plus its position (`seq`), its `content_hash`, the
/// predecessor link (`prev_hash`), and the chained `hash`. This is the shape the live-tail buffer
/// stores and [`verify_chain`] recomputes over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainedEvent {
    /// Monotonic sequence number (1-based; the first event in a chain is `seq = 1`).
    pub seq: u64,
    /// The event's secret-free content fields.
    pub event: AuditEvent,
    /// `sha256_hex(content)` — the stable commitment to `event`'s fields.
    pub content_hash: String,
    /// The predecessor's chained `hash` (the chain link); [`GENESIS_PREV_HASH`] for the first event.
    pub prev_hash: String,
    /// `sha256_hex(content_hash ‖ prev_hash)` — this event's chained hash; the next event's `prev_hash`.
    pub hash: String,
}

impl ChainedEvent {
    /// The durable [`ChainHead`] this event becomes once it is the latest in the chain — exactly
    /// the `(seq, content_hash, prev_hash)` the System DB persists.
    #[must_use]
    pub fn head(&self) -> ChainHead {
        ChainHead {
            seq: self.seq,
            content_hash: self.content_hash.clone(),
            prev_hash: self.prev_hash.clone(),
        }
    }
}

/// The durable **chain head** — the ONLY audit state qfs retains long-term (decision V). Persisting
/// just these three fields is sufficient to continue the chain across restarts: the next event's
/// `prev_hash` is [`ChainHead::hash`], recomputable from `(content_hash, prev_hash)` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    /// The latest event's sequence number (the next event is `seq + 1`).
    pub seq: u64,
    /// The latest event's `content_hash`.
    pub content_hash: String,
    /// The latest event's predecessor link.
    pub prev_hash: String,
}

impl ChainHead {
    /// The head event's chained `hash` — `sha256_hex(content_hash ‖ prev_hash)` — which the NEXT
    /// event chains onto as its `prev_hash`. Recomputed from the stored columns, so the persisted
    /// head row is self-sufficient (no need to retain the raw event content).
    #[must_use]
    pub fn hash(&self) -> String {
        link_hash(&self.content_hash, &self.prev_hash)
    }
}

/// Recompute the hash chain over `events` (the recorded sequence, e.g. read back from the tail
/// buffer or a consumer's durable store) and return the **index of the first divergence**, or
/// `None` if the whole chain verifies. `genesis` is the `prev_hash` the first event must chain
/// onto ([`GENESIS_PREV_HASH`] for a chain that starts at `seq = 1`; a head's hash when verifying a
/// suffix).
///
/// A divergence at index `i` means one of:
/// - **edit** — `events[i].event`'s fields no longer hash to its recorded `content_hash`/`hash`;
/// - **reorder / deletion** — `events[i].prev_hash` no longer equals the running predecessor hash
///   (a dropped or swapped event breaks the link).
///
/// This is the helper the t76 tests use today and the t78 consumer-side seal/verify uses later.
#[must_use]
pub fn verify_chain(events: &[ChainedEvent], genesis: &str) -> Option<usize> {
    let mut prev = genesis.to_string();
    for (i, ev) in events.iter().enumerate() {
        // The predecessor link must match the running hash (catches reorder/deletion).
        if ev.prev_hash != prev {
            return Some(i);
        }
        // The recorded content hash must still recompute from the event's fields (catches edits).
        if ev.content_hash != ev.event.content_hash() {
            return Some(i);
        }
        // The chained hash must recompute from (content_hash, prev_hash) (catches a forged link).
        if ev.hash != link_hash(&ev.content_hash, &ev.prev_hash) {
            return Some(i);
        }
        prev = ev.hash.clone();
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn event(verb: &str, path: &str, committed: bool) -> AuditEvent {
        AuditEvent {
            actor: "cli".to_string(),
            connection: "default".to_string(),
            verb: verb.to_string(),
            path: path.to_string(),
            committed,
            ts: "2026-06-28T00:00:00Z".to_string(),
        }
    }

    /// Build a clean chain of `n` events from genesis (each chained onto the prior's hash).
    fn clean_chain(events: Vec<AuditEvent>) -> Vec<ChainedEvent> {
        let mut prev = GENESIS_PREV_HASH.to_string();
        let mut out = Vec::new();
        for (i, e) in events.into_iter().enumerate() {
            let ce = e.chain(i as u64 + 1, prev.clone());
            prev = ce.hash.clone();
            out.push(ce);
        }
        out
    }

    #[test]
    fn content_hash_is_stable_and_field_sensitive() {
        let a = event("INSERT", "/local/a", true);
        // Stable: identical fields ⇒ identical hash, run to run.
        assert_eq!(a.content_hash(), a.content_hash());
        assert_eq!(
            a.content_hash(),
            event("INSERT", "/local/a", true).content_hash()
        );
        assert_eq!(a.content_hash().len(), 64, "sha256_hex is 64 hex chars");
        // Any field change changes the hash.
        assert_ne!(
            a.content_hash(),
            event("UPSERT", "/local/a", true).content_hash()
        );
        assert_ne!(
            a.content_hash(),
            event("INSERT", "/local/b", true).content_hash()
        );
        assert_ne!(
            a.content_hash(),
            event("INSERT", "/local/a", false).content_hash()
        );
    }

    #[test]
    fn canonical_content_is_injective_across_field_boundaries() {
        // Without length-prefixing these would collide (the "ab|c" vs "a|bc" hazard). The
        // length-prefixed encoding keeps them distinct.
        let mut x = event("INSERT", "/local/a", true);
        x.connection = "ab".to_string();
        x.verb = "c".to_string();
        let mut y = event("INSERT", "/local/a", true);
        y.connection = "a".to_string();
        y.verb = "bc".to_string();
        assert_ne!(x.content_hash(), y.content_hash());
    }

    #[test]
    fn a_clean_chain_verifies() {
        let chain = clean_chain(vec![
            event("INSERT", "/local/a", true),
            event("UPSERT", "/local/b", true),
            event("REMOVE", "/local/c", true),
        ]);
        assert_eq!(verify_chain(&chain, GENESIS_PREV_HASH), None);
        // The head derived from the last event is self-sufficient: its hash continues the chain.
        let head = chain.last().unwrap().head();
        assert_eq!(head.hash(), chain.last().unwrap().hash);
    }

    #[test]
    fn an_edit_is_detected_at_the_edited_index() {
        let mut chain = clean_chain(vec![
            event("INSERT", "/local/a", true),
            event("UPSERT", "/local/b", true),
            event("REMOVE", "/local/c", true),
        ]);
        // Tamper the MIDDLE event's payload without re-deriving its hashes.
        chain[1].event.path = "/local/HACKED".to_string();
        assert_eq!(verify_chain(&chain, GENESIS_PREV_HASH), Some(1));
    }

    #[test]
    fn a_deletion_is_detected_at_the_break() {
        let chain = clean_chain(vec![
            event("INSERT", "/local/a", true),
            event("UPSERT", "/local/b", true),
            event("REMOVE", "/local/c", true),
        ]);
        // Drop the middle event: index 1 (formerly the 3rd) now links onto a hash that is no
        // longer its predecessor.
        let pruned = vec![chain[0].clone(), chain[2].clone()];
        assert_eq!(verify_chain(&pruned, GENESIS_PREV_HASH), Some(1));
    }

    #[test]
    fn a_reorder_is_detected() {
        let chain = clean_chain(vec![
            event("INSERT", "/local/a", true),
            event("UPSERT", "/local/b", true),
            event("REMOVE", "/local/c", true),
        ]);
        // Swap the last two: index 1 now carries a prev_hash that does not match index 0's hash.
        let reordered = vec![chain[0].clone(), chain[2].clone(), chain[1].clone()];
        assert_eq!(verify_chain(&reordered, GENESIS_PREV_HASH), Some(1));
    }

    #[test]
    fn content_carries_no_secret_or_row_data() {
        // The canonical content is built ONLY from the six metadata fields — there is no field for
        // a secret or a row payload, so an event cannot carry one. This pins the metadata-only
        // boundary structurally: the serialized bytes are exactly the labelled fields.
        let e = event("INSERT", "/local/a", true);
        let bytes = e.content();
        // The domain tag + each field's bytes appear; nothing else can (no payload field exists).
        let as_str = String::from_utf8_lossy(&bytes);
        assert!(as_str.contains("cli"));
        assert!(as_str.contains("/local/a"));
        assert!(as_str.contains("INSERT"));
        // A would-be secret never appears because there is nowhere to put it.
        assert!(!as_str.contains("super-secret-token"));
    }
}
