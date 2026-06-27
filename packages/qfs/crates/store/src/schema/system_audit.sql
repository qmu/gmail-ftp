-- System DB — migration #2 (roadmap decision V / §4.6, ticket t76): the hash-chained audit
-- stream's DURABLE state.
--
-- qfs EMITS the audit stream; it does NOT retain the whole log (decision V — retention/period is
-- the consumer's concern: t77 sinks, t78 external WORM/transparency sealing). Only TWO things are
-- durable here, and NEITHER grows unboundedly:
--   1. the chain HEAD — to continue the tamper-evident hash chain across restarts, and
--   2. a BOUNDED live-tail buffer — the recent events backing the /sys/audit live view (t53).
--
-- Appended as a NEW migration version (#2). Migration #1's body stays FROZEN (the checksum guard
-- forbids editing a shipped migration in place).

-- The chain head: exactly ONE row (id = 1, enforced by the CHECK). Holds the latest event's
-- sequence number + its content hash + its predecessor link — exactly the three columns t76 names.
-- This is self-sufficient to continue the chain: the next event's prev_hash is
-- sha256_hex(content_hash || prev_hash), recomputable from this row alone.
CREATE TABLE IF NOT EXISTS audit_chain_head (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    seq          INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    prev_hash    TEXT NOT NULL
);

-- The bounded live-tail buffer: the most-recent emitted events the /sys/audit live view reads
-- (t53 wires the read surface). The emit path trims older rows so this never becomes the full log
-- (decision V). METADATA ONLY — actor/connection/verb/path/committed/ts + the chain hashes; never
-- a secret, never row data (the same boundary `describe` enforces, §3.2/§4.6).
CREATE TABLE IF NOT EXISTS audit_tail (
    seq          INTEGER PRIMARY KEY,
    actor        TEXT NOT NULL,
    connection   TEXT NOT NULL,
    verb         TEXT NOT NULL,
    path         TEXT NOT NULL,
    committed    INTEGER NOT NULL,
    ts           TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    prev_hash    TEXT NOT NULL,
    hash         TEXT NOT NULL
);
