-- System DB — migration #10 (roadmap §2.4 / M5, ticket t59): the deployment SETTINGS key/value the
-- `/sys/settings` admin path reads and (gated) writes.
--
-- The selectable AI **safety mode** (decision J / roadmap §2.4) is stored HERE as data — one row,
-- `key = 'safety_mode'`, `value` one of `autonomous-in-policy` / `approve-everything` /
-- `policy-only`. Storing the active mode as a `/sys/settings` row (rather than a flag or a keyword)
-- keeps it describable / previewable / committable through the SAME one-engine-three-faces surface
-- as any other relation: `FROM /sys/settings` reads it, a gated `INSERT INTO /sys/settings` (upsert
-- on `key`) sets it, and every such mutation appends a t76 audit row (administration observes
-- itself). A generic KV shape so a later operator setting adds a row, not a column.
--
-- The setting CONFIGURES the safety floor; it never lowers it: an unset/garbled value resolves to
-- the safe default (autonomous-in-policy — irreversible needs approval), and the policy gate +
-- irreversible-ack floor hold in every mode (see `crates/core/src/security.rs`).
--
-- APPEND-ONLY: migrations #1–#9 are FROZEN — the checksum guard forbids editing a shipped migration
-- in place; this ships as a NEW version (#10). The rusqlite read/write that fills these columns
-- lives in the binary-injected `SysBackend` (`crates/qfs/src/sys.rs`); this migration only declares
-- the shape. AUTHORIZATION NOTE: a `/sys/settings` write is high-privilege (super-admin); the
-- local-super-admin vs. project-admin split is deliberately NOT baked in yet (roadmap §3.4).
CREATE TABLE IF NOT EXISTS sys_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
