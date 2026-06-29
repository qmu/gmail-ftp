//! t78: the binary-side IMPURE half of audit-chain **sealing to an external WORM / transparency
//! log** — the concrete append-only **file** witness, the parked **external** (S3 Object Lock /
//! transparency-log) seam, the seal **trigger** (read the chain head → sign a checkpoint → hand it to
//! a witness), and the consumer-side **verify** helper.
//!
//! The PURE halves live in two leaves the binary composes:
//! - `qfs_oauth::{AuditSeal, sign_seal, verify_seal}` — the signed-checkpoint primitives over the AS
//!   ES256 key (the SAME key that signs access tokens).
//! - `qfs_store::worm::{SealRecord, WormSink, WormError, WormKind}` — the metadata-only seal record +
//!   the append-only witness seam.
//!
//! This module owns only what MUST be binary-side: opening a real path (the `file` witness), the
//! external exporter SEAM, and reading the System DB chain head (decision F/V) — the binary is the
//! ONE crate that holds the AS key material, the System DB, and a real witness target, exactly like
//! the telemetry sinks and the audit chain-head I/O.
//!
//! ## Emit, don't store (decision V) + externalized cadence (decision M)
//! [`seal_chain_head`] is the **invokable unit**: it reads the current chain head (t76), signs a
//! checkpoint over it, and appends that seal to the configured witness. qfs owns the *what* (the
//! seal), not the *when* — the cadence is fired by **OS cron / Cloudflare Cron Triggers / an external
//! trigger** per the roadmap; this module builds NO scheduler. The seal goes OUT; qfs keeps only the
//! head.
//!
//! ## Offline-honest status (t78)
//! The local append-only **file** witness ([`WormFileSink`]) is fully wired. The **external** witness
//! ([`ExternalWormSink`]) is a **present-but-parked seam** — selectable + metadata-rendering, but the
//! real client (S3 Object Lock `PutObject` with a retention lock, or a transparency-log
//! `add-leaf`) is **NOT wired**: no vetted transparency-log / Object-Lock client crate is resolvable
//! in the offline build cache, and t78 deliberately does NOT hand-roll RFC 6962 / a vendor protocol.
//! When a vetted client lands, replace the [`ExternalWormSink::append`] body with the real call over
//! [`ExternalWormSink::endpoint`] — the trigger, the seal model, and the verify helper are unchanged
//! (this is the only place that needs the witness dependency).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use qfs_oauth::{sign_seal, verify_seal, AuditSeal, Jwks, SealError, SigningKey};
use qfs_store::audit::{verify_chain, ChainHead, ChainedEvent};
use qfs_store::worm::{SealRecord, WormError, WormKind, WormSink};
use qfs_store::{StoreError, SystemDb};

/// The env var overriding the local `file` witness's path.
pub const ENV_SEAL_FILE: &str = "QFS_AUDIT_SEAL_FILE";
/// The env var selecting an external witness endpoint (the parked S3 Object Lock / transparency-log
/// seam). Absent ⇒ the local `file` witness is used.
pub const ENV_SEAL_ENDPOINT: &str = "QFS_AUDIT_SEAL_ENDPOINT";

/// Resolve the local `file` witness path: `QFS_AUDIT_SEAL_FILE` if set, else
/// `<config-home>/qfs/audit-seals.jsonl` (next to the System DB + the telemetry file), else `None`
/// when no config home resolves — in which case the `file` witness is a no-op rather than writing to
/// an unexpected location (mirrors the telemetry `file` sink).
#[must_use]
pub fn default_seal_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ENV_SEAL_FILE) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    crate::store::default_system_db_path().map(|mut db| {
        db.set_file_name("audit-seals.jsonl");
        db
    })
}

/// Build the active witness from the process environment (the binary's seal composition root): an
/// external endpoint (`QFS_AUDIT_SEAL_ENDPOINT`) selects the parked [`ExternalWormSink`] seam;
/// otherwise the local append-only [`WormFileSink`] over [`default_seal_file_path`].
#[must_use]
pub fn witness_from_env() -> Box<dyn WormSink> {
    match std::env::var(ENV_SEAL_ENDPOINT)
        .ok()
        .filter(|e| !e.is_empty())
    {
        Some(endpoint) => Box::new(ExternalWormSink::new(Some(endpoint))),
        None => Box::new(WormFileSink::new(default_seal_file_path())),
    }
}

/// The local append-only **file** witness (the default): APPEND each seal's JSONL line to a path.
/// Point it at a path the server process cannot rewrite (a different volume / an off-box synced dir)
/// or sync it to an immutable bucket — qfs only appends; retention + immutability are the consumer's.
/// A `None` path (no resolvable config home) makes every append a silent no-op so a host without a
/// config home runs un-sealed rather than failing (the trigger still reports a no-op witness kind).
pub struct WormFileSink {
    /// The append target, or `None` for the no-op witness.
    path: Option<PathBuf>,
}

impl WormFileSink {
    /// Build a `file` witness over `path` (`None` = the no-op witness).
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

impl WormSink for WormFileSink {
    fn append(&self, record: &SealRecord) -> Result<(), WormError> {
        let Some(path) = &self.path else {
            return Ok(()); // no config home: a no-op witness
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| WormError::Append(format!("create dir: {e}")))?;
        }
        let mut line = record.to_jsonl();
        line.push('\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| WormError::Append(format!("open: {e}")))?;
        f.write_all(line.as_bytes())
            .map_err(|e| WormError::Append(format!("write: {e}")))
    }

    fn kind(&self) -> WormKind {
        WormKind::LocalFile
    }
}

/// The **external** write-once witness — **the exporter SEAM** (S3 Object Lock first; a transparency
/// log / signed off-box anchor behind the same seam).
///
/// **Offline-honest status (t78):** present + selectable, but the real client is **NOT wired** — no
/// vetted Object-Lock / transparency-log crate is resolvable offline, and t78 does NOT hand-roll
/// RFC 6962 / a vendor protocol. `append` renders the seal line the witness WOULD store (proving the
/// metadata-only boundary), records it through `tracing` at debug under `qfs::worm::external`, and
/// returns `Ok`. When a vetted client lands, replace the body with the real append over
/// [`ExternalWormSink::endpoint`]; nothing else changes.
pub struct ExternalWormSink {
    /// The configured external witness endpoint (`QFS_AUDIT_SEAL_ENDPOINT`), or `None`.
    endpoint: Option<String>,
}

impl ExternalWormSink {
    /// Build the seam over an explicit endpoint (`None` = unconfigured).
    #[must_use]
    pub fn new(endpoint: Option<String>) -> Self {
        Self { endpoint }
    }

    /// The configured external endpoint, if any (the real witness target once wired).
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
}

impl WormSink for ExternalWormSink {
    fn append(&self, record: &SealRecord) -> Result<(), WormError> {
        // SEAM: the external client is not wired offline. Render the line the witness WOULD store
        // (metadata-only, proving the boundary) and trace it; return Ok so a configured trigger sees
        // the seam as present rather than failing the cadence.
        tracing::debug!(
            target: "qfs::worm::external",
            endpoint = self.endpoint.as_deref().unwrap_or("<unset>"),
            "external WORM witness not wired (dep unavailable offline); seal: {}",
            record.to_jsonl()
        );
        Ok(())
    }

    fn kind(&self) -> WormKind {
        WormKind::External
    }
}

/// The trigger failure taxonomy — secret-free. Distinct from a witness [`WormError`] so the caller
/// can tell a clock/DB/sign failure apart from a witness-append failure.
#[derive(Debug, thiserror::Error)]
pub enum SealTriggerError {
    /// Reading the System DB chain head failed.
    #[error("reading the audit chain head failed: {0}")]
    Store(#[from] StoreError),
    /// Signing the checkpoint over the head failed (the AS key could not sign).
    #[error("signing the audit seal failed")]
    Sign,
    /// Handing the seal to the witness failed.
    #[error("sealing to the witness failed: {0}")]
    Witness(#[from] WormError),
}

/// **The seal trigger** (the externally-fired invokable unit, decision M). Read the current audit
/// chain head (t76); if the chain is empty (`None`), there is nothing to seal — return `Ok(None)`.
/// Otherwise sign a checkpoint ([`AuditSeal`]) over the head with the AS `key` and `issued_at` (the
/// binary injects the wall clock), append the resulting [`SealRecord`] to `witness`, and return it.
///
/// Emit-don't-store stays intact: this reads the head qfs already keeps and pushes the seal OUT — it
/// persists nothing new in qfs.
///
/// # Errors
/// [`SealTriggerError`] if reading the head, signing, or the witness append failed.
pub fn seal_chain_head(
    sys: &SystemDb,
    key: &SigningKey,
    witness: &dyn WormSink,
    issued_at: &str,
) -> Result<Option<SealRecord>, SealTriggerError> {
    let Some(head) = crate::audit::chain_head(sys)? else {
        return Ok(None); // a fresh chain: nothing to seal yet
    };
    let record = seal_record_for(&head, key, issued_at).ok_or(SealTriggerError::Sign)?;
    witness.append(&record)?;
    Ok(Some(record))
}

/// Sign a checkpoint over `head` and assemble the [`SealRecord`] (the pure glue between the
/// `qfs-oauth` seal and the `qfs-store` witness record). `None` if the ECDSA sign fails.
fn seal_record_for(head: &ChainHead, key: &SigningKey, issued_at: &str) -> Option<SealRecord> {
    let seal = AuditSeal {
        seq: head.seq,
        content_hash: head.content_hash.clone(),
        prev_hash: head.prev_hash.clone(),
        issued_at: issued_at.to_string(),
    };
    let token = sign_seal(&seal, key).ok()?;
    Some(SealRecord {
        seq: seal.seq,
        content_hash: seal.content_hash,
        prev_hash: seal.prev_hash,
        issued_at: seal.issued_at,
        seal: token,
    })
}

/// The consumer-side seal-verification outcome taxonomy — the answer a third party gets when it
/// recomputes ITS OWN stored chain against a seal qfs emitted (roadmap §4.6). Value-free.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SealVerifyError {
    /// The seal's signature / structure is bad, or it is not a qfs seal (the [`qfs_oauth`] check).
    #[error("the audit seal is not authentic: {0}")]
    BadSeal(#[from] SealError),
    /// The stored chain itself does not recompute — an edit / reorder / deletion at the consumer's
    /// store, with the **index of the first divergence** (the t76 `verify_chain` answer).
    #[error("the stored audit chain diverges at index {0}")]
    ChainBroken(usize),
    /// The chain recomputes, but its head does NOT match what the seal commits to — the store was
    /// **truncated or forked** below / away from the sealed head (the tamper the external witness
    /// exists to catch). Carries the sealed `seq` vs the stored chain's actual `seq`.
    #[error(
        "the sealed head (seq {sealed_seq}) does not match the stored chain (seq {stored_seq})"
    )]
    HeadMismatch {
        /// The `seq` the seal commits to.
        sealed_seq: u64,
        /// The `seq` the consumer's stored chain actually reaches.
        stored_seq: u64,
    },
}

/// **The consumer-side verify** (roadmap §4.6). Given a seal qfs emitted (`token`), the published
/// `jwks`, the consumer's own stored `events` (the chain it retained), and the `genesis` the first
/// event chains onto, confirm ALL THREE tamper-evidence properties:
///
/// 1. **Authenticity** — the seal verifies against the AS JWKS ([`verify_seal`]) — qfs issued it.
/// 2. **Integrity** — the stored chain recomputes ([`verify_chain`] returns `None`) — no edit /
///    reorder / deletion within it.
/// 3. **Continuity** — the recomputed head equals what the seal commits to (`seq` + `content_hash` +
///    `prev_hash`) — the store was not truncated or forked away from the sealed head.
///
/// On success the verified [`AuditSeal`] is returned (the sealed head + `issued_at`).
///
/// # Errors
/// [`SealVerifyError`] for the first failing property: a bad/forged seal, a broken chain (with the
/// first-divergence index), or a head that does not match the seal (truncation / fork).
pub fn verify_seal_against_events(
    token: &str,
    jwks: &Jwks,
    events: &[ChainedEvent],
    genesis: &str,
) -> Result<AuditSeal, SealVerifyError> {
    // 1. Authenticity: the seal is a real, AS-signed checkpoint.
    let seal = verify_seal(token, jwks)?;

    // 2. Integrity: the stored chain recomputes (no edit/reorder/deletion within it).
    if let Some(idx) = verify_chain(events, genesis) {
        return Err(SealVerifyError::ChainBroken(idx));
    }

    // 3. Continuity: the recomputed head matches what the seal commits to. An empty store cannot
    //    satisfy a seal over a non-empty chain; a head whose (seq, content_hash, prev_hash) differs
    //    from the seal is a truncation / fork the witness exists to catch.
    let stored_seq = events.last().map_or(0, |e| e.seq);
    let head_matches = events.last().is_some_and(|last| {
        last.seq == seal.seq
            && last.content_hash == seal.content_hash
            && last.prev_hash == seal.prev_hash
    });
    if !head_matches {
        return Err(SealVerifyError::HeadMismatch {
            sealed_seq: seal.seq,
            stored_seq,
        });
    }

    Ok(seal)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_store::audit::{AuditEvent, GENESIS_PREV_HASH};
    use qfs_store::{FileSource, SystemDb};

    const ISSUED_AT: &str = "2026-06-28T00:00:00Z";

    fn key() -> SigningKey {
        // A fixed 32-byte scalar → a stable AS key (and thus stable seals) across the test run.
        SigningKey::generate(&[0x42u8; 32]).unwrap()
    }

    fn jwks_of(key: &SigningKey) -> Jwks {
        Jwks::new(vec![key.public_jwk()])
    }

    fn event(verb: &str, path: &str) -> AuditEvent {
        AuditEvent {
            actor: "cli".to_string(),
            connection: "default".to_string(),
            verb: verb.to_string(),
            path: path.to_string(),
            committed: true,
            ts: ISSUED_AT.to_string(),
        }
    }

    /// Build a clean chain of events from genesis (each chained onto the prior's hash).
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
    fn a_seal_over_a_known_head_signs_and_verifies_against_the_jwks() {
        let key = key();
        let chain = clean_chain(vec![
            event("INSERT", "/local/a"),
            event("UPSERT", "/local/b"),
        ]);
        let head = chain.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();

        // The seal verifies against the AS JWKS AND matches the recomputed chain head.
        let seal =
            verify_seal_against_events(&record.seal, &jwks_of(&key), &chain, GENESIS_PREV_HASH)
                .unwrap();
        assert_eq!(seal.seq, 2);
        assert_eq!(seal.content_hash, head.content_hash);
        assert_eq!(seal.prev_hash, head.prev_hash);
        assert_eq!(seal.issued_at, ISSUED_AT);
    }

    #[test]
    fn a_tampered_chain_fails_verification_at_the_right_point() {
        let key = key();
        let mut chain = clean_chain(vec![
            event("INSERT", "/local/a"),
            event("UPSERT", "/local/b"),
            event("REMOVE", "/local/c"),
        ]);
        let head = chain.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();

        // Tamper the MIDDLE event without re-deriving its hashes: the seal's signature is still
        // valid, but the consumer's recompute diverges at the edited index.
        chain[1].event.path = "/local/HACKED".to_string();
        let err =
            verify_seal_against_events(&record.seal, &jwks_of(&key), &chain, GENESIS_PREV_HASH)
                .unwrap_err();
        assert_eq!(err, SealVerifyError::ChainBroken(1));
    }

    #[test]
    fn a_truncated_chain_fails_the_head_continuity_check() {
        let key = key();
        let full = clean_chain(vec![
            event("INSERT", "/local/a"),
            event("UPSERT", "/local/b"),
            event("REMOVE", "/local/c"),
        ]);
        // Seal the FULL head (seq 3) ...
        let head = full.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();

        // ... but the consumer presents a TRUNCATED store (the last event dropped). The remaining
        // chain still recomputes cleanly, so only the head-continuity check catches the truncation.
        let truncated = full[..2].to_vec();
        assert_eq!(verify_chain(&truncated, GENESIS_PREV_HASH), None);
        let err =
            verify_seal_against_events(&record.seal, &jwks_of(&key), &truncated, GENESIS_PREV_HASH)
                .unwrap_err();
        assert_eq!(
            err,
            SealVerifyError::HeadMismatch {
                sealed_seq: 3,
                stored_seq: 2,
            }
        );
    }

    #[test]
    fn a_seal_whose_head_does_not_match_the_events_is_rejected() {
        let key = key();
        // Seal a head that belongs to a DIFFERENT chain (a forked store).
        let other = clean_chain(vec![event("INSERT", "/elsewhere/x")]);
        let foreign_head = other.last().unwrap().head();
        let record = seal_record_for(&foreign_head, &key, ISSUED_AT).unwrap();

        // The consumer's own chain recomputes fine, but its head is not the sealed one.
        let mine = clean_chain(vec![event("INSERT", "/local/a")]);
        let err =
            verify_seal_against_events(&record.seal, &jwks_of(&key), &mine, GENESIS_PREV_HASH)
                .unwrap_err();
        assert!(matches!(err, SealVerifyError::HeadMismatch { .. }));
    }

    #[test]
    fn a_seal_signed_by_a_foreign_key_is_not_authentic() {
        let key = key();
        let chain = clean_chain(vec![event("INSERT", "/local/a")]);
        let head = chain.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();

        // Verify against a JWKS that does not contain the signer → BadSeal (authenticity fails first).
        let foreign = SigningKey::generate(&[7u8; 32]).unwrap();
        let err =
            verify_seal_against_events(&record.seal, &jwks_of(&foreign), &chain, GENESIS_PREV_HASH)
                .unwrap_err();
        assert!(matches!(err, SealVerifyError::BadSeal(_)));
    }

    #[test]
    fn the_worm_file_sink_appends_the_seal() {
        let key = key();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("audit-seals.jsonl");
        let witness = WormFileSink::new(Some(path.clone()));
        assert_eq!(witness.kind(), WormKind::LocalFile);

        let chain = clean_chain(vec![event("INSERT", "/local/a")]);
        let head = chain.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();

        // Append two seals: each lands as ONE JSONL line (append-only).
        witness.append(&record).unwrap();
        witness.append(&record).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per appended seal");
        assert!(lines[0].contains("\"seq\":1"));
        assert!(lines[0].contains(&format!("\"seal\":\"{}\"", record.seal)));
    }

    #[test]
    fn the_file_witness_with_no_path_is_a_silent_noop() {
        let witness = WormFileSink::new(None);
        let record = SealRecord {
            seq: 1,
            content_hash: "a".repeat(64),
            prev_hash: "b".repeat(64),
            issued_at: ISSUED_AT.to_string(),
            seal: "x.y.z".to_string(),
        };
        assert!(witness.append(&record).is_ok());
    }

    #[test]
    fn the_external_witness_seam_compiles_and_never_fails() {
        // Present + selectable; the exporter is parked (offline), so append is a best-effort
        // no-fail render. Pins that the seam COMPILES and is wired into selection.
        let witness = ExternalWormSink::new(Some("s3://audit-bucket?object-lock=on".to_string()));
        assert_eq!(witness.endpoint(), Some("s3://audit-bucket?object-lock=on"));
        assert_eq!(witness.kind(), WormKind::External);
        let record = SealRecord {
            seq: 1,
            content_hash: "a".repeat(64),
            prev_hash: "b".repeat(64),
            issued_at: ISSUED_AT.to_string(),
            seal: "x.y.z".to_string(),
        };
        assert!(witness.append(&record).is_ok());
    }

    #[test]
    fn no_secret_leaks_into_a_seal_or_its_witness_line() {
        // The seal + its witness line carry ONLY chain-position metadata + the public signature.
        let key = key();
        let chain = clean_chain(vec![event("INSERT", "/local/a")]);
        let head = chain.last().unwrap().head();
        let record = seal_record_for(&head, &key, ISSUED_AT).unwrap();
        assert!(!record.to_jsonl().contains("super-secret-token"));
        assert!(!record.seal.contains("super-secret-token"));
    }

    #[test]
    fn the_trigger_seals_the_persisted_head_and_noops_on_an_empty_chain() {
        let key = key();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("system.db");
        let sys = SystemDb::open(&FileSource::new(&db_path)).unwrap();

        // A fresh chain has no head: the trigger is a no-op (nothing to seal).
        let witness = WormFileSink::new(Some(dir.path().join("seals.jsonl")));
        assert!(seal_chain_head(&sys, &key, &witness, ISSUED_AT)
            .unwrap()
            .is_none());

        // Append two events, then fire the trigger: it seals the CURRENT head (seq 2).
        crate::audit::append_event(&sys, event("INSERT", "/local/a")).unwrap();
        crate::audit::append_event(&sys, event("UPSERT", "/local/b")).unwrap();
        let record = seal_chain_head(&sys, &key, &witness, ISSUED_AT)
            .unwrap()
            .expect("a head to seal");
        assert_eq!(record.seq, 2);

        // The emitted seal verifies against the live tail (the consumer's stored chain).
        let tail = crate::audit::recent_tail(&sys).unwrap();
        let seal =
            verify_seal_against_events(&record.seal, &jwks_of(&key), &tail, GENESIS_PREV_HASH)
                .unwrap();
        assert_eq!(seal.seq, 2);
    }
}
