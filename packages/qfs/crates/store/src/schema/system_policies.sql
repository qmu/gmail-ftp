-- System DB — migration #7 (roadmap §3.4 / M3, ticket t53): the policy GRANTS the `/sys/policies`
-- admin path reads and writes.
--
-- `/sys/policies` is the path façade over the policy model (the extended ACL language is a later
-- ticket): one row per grant — a `name`, the `allow`ed verb, and the driver/path `target` glob the
-- grant applies to. A super-admin `INSERT INTO /sys/policies VALUES (...)` lands a row here
-- transactionally, and every such mutation also appends a t76 audit row (administration observes
-- itself). The READ views (`FROM /sys/policies`) project these columns directly.
--
-- APPEND-ONLY: migrations #1–#6 are FROZEN — the checksum guard forbids editing a shipped migration
-- in place; this ships as a NEW version (#7). The rusqlite read/write that fills these columns lives
-- in the binary-injected `SysBackend` (qfs-store owns the connection); this migration only declares
-- the shape. AUTHENTICATION/AUTHORIZATION NOTE: a row here is a grant record for the policy engine;
-- the local-super-admin vs. project-admin split is deliberately NOT baked in yet (roadmap §3.4).
CREATE TABLE IF NOT EXISTS sys_policies (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    allow      TEXT,
    target     TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
