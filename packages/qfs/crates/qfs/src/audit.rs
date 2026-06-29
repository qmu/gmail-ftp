//! t76: the binary-side IMPURE half of the hash-chained audit stream — reading/writing the durable
//! chain HEAD and the bounded live-tail buffer in the System DB.
//!
//! The PURE half (the event record, its canonical content hash, the chain link, and the
//! recompute/verify helper) lives in `qfs_store::audit`. This module owns only what MUST be
//! binary-side: the System DB I/O. The binary is the ONE crate that opens a real DB path
//! (decision F + the dep-direction guard — `qfs-store` is a sync leaf; nothing in the spine names a
//! file), so the chain-head read/write lives here, exactly like t43's `SqliteSecrets` backend.
//!
//! ## What this does on each committed effect
//!
//! [`append_event`] threads one event onto the chain atomically: read the head, compute the next
//! [`ChainedEvent`] (its `prev_hash` is the head's recomputed hash, or [`GENESIS_PREV_HASH`] for the
//! first event), then persist the new head + append the tail row + trim the tail to its cap — all in
//! ONE transaction so a crash can never leave a torn chain. qfs retains ONLY the head durably
//! (decision V); the tail is a bounded live-view buffer, not the full log.

use qfs_store::audit::{AuditEvent, ChainHead, ChainedEvent, GENESIS_PREV_HASH};
use qfs_store::{StoreError, SystemDb};
use rusqlite::OptionalExtension;

/// The bounded live-tail buffer's capacity — the most-recent events the `/sys/audit` view reads.
/// qfs EMITS the stream and does not retain the whole log (decision V), so the tail is trimmed to
/// this many rows on every append; durable retention is the consumer's concern (t77/t78).
pub const TAIL_CAP: u64 = 256;

/// Read the durable [`ChainHead`], or `None` for a fresh chain (no event recorded yet).
pub fn chain_head(sys: &SystemDb) -> Result<Option<ChainHead>, StoreError> {
    let head = sys
        .db()
        .conn()
        .query_row(
            "SELECT seq, content_hash, prev_hash FROM audit_chain_head WHERE id = 1",
            [],
            |r| {
                Ok(ChainHead {
                    seq: r.get::<_, i64>(0)? as u64,
                    content_hash: r.get(1)?,
                    prev_hash: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)?;
    Ok(head)
}

/// Append `event` to the hash chain and return the recorded [`ChainedEvent`].
///
/// Atomic: the head read, the head upsert, the tail insert, and the tail trim all run inside ONE
/// transaction (over `&Connection` via `unchecked_transaction`, since the binary holds the System
/// DB by shared reference), so the durable head and the live tail never disagree even across a
/// crash. The new event's `prev_hash` is the previous head's recomputed hash, anchoring to
/// [`GENESIS_PREV_HASH`] when the chain is empty.
pub fn append_event(sys: &SystemDb, event: AuditEvent) -> Result<ChainedEvent, StoreError> {
    let tx = sys
        .db()
        .conn()
        .unchecked_transaction()
        .map_err(StoreError::from)?;

    // The head INSIDE the transaction (so a concurrent appender cannot interleave between read and
    // write — SQLite serialises writers, and the tx holds the write lock once we write).
    let head: Option<ChainHead> = tx
        .query_row(
            "SELECT seq, content_hash, prev_hash FROM audit_chain_head WHERE id = 1",
            [],
            |r| {
                Ok(ChainHead {
                    seq: r.get::<_, i64>(0)? as u64,
                    content_hash: r.get(1)?,
                    prev_hash: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)?;

    let (seq, prev_hash) = match head {
        Some(h) => (h.seq + 1, h.hash()),
        None => (1, GENESIS_PREV_HASH.to_string()),
    };
    let chained = event.chain(seq, prev_hash);

    // Persist the new head (the ONE durable row, decision V).
    let new_head = chained.head();
    tx.execute(
        "INSERT INTO audit_chain_head (id, seq, content_hash, prev_hash) VALUES (1, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET seq = excluded.seq, content_hash = excluded.content_hash, \
         prev_hash = excluded.prev_hash",
        rusqlite::params![
            new_head.seq as i64,
            new_head.content_hash,
            new_head.prev_hash
        ],
    )
    .map_err(StoreError::from)?;

    // Append the live-tail row (metadata only — never a secret, never row data).
    tx.execute(
        "INSERT INTO audit_tail \
         (seq, actor, connection, verb, path, committed, ts, content_hash, prev_hash, hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            chained.seq as i64,
            chained.event.actor,
            chained.event.connection,
            chained.event.verb,
            chained.event.path,
            i64::from(chained.event.committed),
            chained.event.ts,
            chained.content_hash,
            chained.prev_hash,
            chained.hash,
        ],
    )
    .map_err(StoreError::from)?;

    // Trim the tail to its cap: keep only the most-recent TAIL_CAP rows (decision V — qfs does not
    // retain the whole log). Deleting by seq is exact because seq is monotonic.
    tx.execute(
        "DELETE FROM audit_tail WHERE seq <= ?1 - ?2",
        rusqlite::params![chained.seq as i64, TAIL_CAP as i64],
    )
    .map_err(StoreError::from)?;

    tx.commit().map_err(StoreError::from)?;
    Ok(chained)
}

/// Read the bounded live-tail buffer back as the recorded [`ChainedEvent`] sequence (ascending by
/// `seq`) — the recent events the `/sys/audit` live view exposes (t53 wires the read surface) and
/// the sequence `qfs_store::audit::verify_chain` recomputes over.
pub fn recent_tail(sys: &SystemDb) -> Result<Vec<ChainedEvent>, StoreError> {
    let conn = sys.db().conn();
    let mut stmt = conn
        .prepare(
            "SELECT seq, actor, connection, verb, path, committed, ts, content_hash, prev_hash, hash \
             FROM audit_tail ORDER BY seq",
        )
        .map_err(StoreError::from)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ChainedEvent {
                seq: r.get::<_, i64>(0)? as u64,
                event: AuditEvent {
                    actor: r.get(1)?,
                    connection: r.get(2)?,
                    verb: r.get(3)?,
                    path: r.get(4)?,
                    committed: r.get::<_, i64>(5)? != 0,
                    ts: r.get(6)?,
                },
                content_hash: r.get(7)?,
                prev_hash: r.get(8)?,
                hash: r.get(9)?,
            })
        })
        .map_err(StoreError::from)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StoreError::from)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_store::audit::{verify_chain, GENESIS_PREV_HASH};
    use qfs_store::{FileSource, SystemDb};

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

    #[test]
    fn append_threads_a_verifiable_chain_and_advances_the_head() {
        let dir = tempfile::tempdir().unwrap();
        let sys = SystemDb::open(&FileSource::new(dir.path().join("system.db"))).unwrap();

        // No head before the first event.
        assert!(chain_head(&sys).unwrap().is_none());

        let a = append_event(&sys, event("INSERT", "/local/a", true)).unwrap();
        assert_eq!(a.seq, 1);
        assert_eq!(a.prev_hash, GENESIS_PREV_HASH);

        let b = append_event(&sys, event("UPSERT", "/local/b", true)).unwrap();
        assert_eq!(b.seq, 2);
        // The second event chains onto the first's hash (the head's recomputed hash).
        assert_eq!(b.prev_hash, a.hash);

        let c = append_event(&sys, event("REMOVE", "/local/c", false)).unwrap();
        assert_eq!(c.seq, 3);

        // The durable head tracks the latest event.
        let head = chain_head(&sys).unwrap().unwrap();
        assert_eq!(head.seq, 3);
        assert_eq!(head.hash(), c.hash);

        // The live tail reads back a clean, verifiable chain from genesis.
        let tail = recent_tail(&sys).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(verify_chain(&tail, GENESIS_PREV_HASH), None);
    }

    #[test]
    fn the_chain_continues_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system.db");
        let first_hash = {
            let sys = SystemDb::open(&FileSource::new(&path)).unwrap();
            append_event(&sys, event("INSERT", "/local/a", true))
                .unwrap()
                .hash
        };
        // Reopen: the head persisted, so the next event chains onto the prior hash (seq advances).
        let sys2 = SystemDb::open(&FileSource::new(&path)).unwrap();
        let b = append_event(&sys2, event("UPSERT", "/local/b", true)).unwrap();
        assert_eq!(b.seq, 2);
        assert_eq!(b.prev_hash, first_hash);
        assert_eq!(
            verify_chain(&recent_tail(&sys2).unwrap(), GENESIS_PREV_HASH),
            None
        );
    }

    #[test]
    fn the_live_tail_is_bounded_to_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let sys = SystemDb::open(&FileSource::new(dir.path().join("system.db"))).unwrap();
        // Emit more than the cap; the tail keeps only the most-recent TAIL_CAP rows, but the head
        // keeps advancing (qfs emits the stream, retains only the head + a bounded tail).
        let total = TAIL_CAP + 10;
        for i in 0..total {
            append_event(&sys, event("INSERT", &format!("/local/{i}"), true)).unwrap();
        }
        let tail = recent_tail(&sys).unwrap();
        assert_eq!(tail.len() as u64, TAIL_CAP);
        assert_eq!(chain_head(&sys).unwrap().unwrap().seq, total);
        // The retained suffix still verifies against the predecessor link the first kept row carries.
        let genesis = tail[0].prev_hash.clone();
        assert_eq!(verify_chain(&tail, &genesis), None);
    }
}
