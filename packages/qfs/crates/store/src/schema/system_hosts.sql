-- System DB — migration #13 (EPIC 20260702120000 / ADR 0008 §1 — the multi-host account model):
-- the CLIENT-SIDE hosts registry (the gh-CLI hosts file, made a table).
--
-- APPEND-ONLY: migrations #1..#12 are FROZEN (the checksum guard forbids editing a shipped
-- migration); this table ships as a NEW version.
--
-- ADR 0008 §1: the CLI is a multi-host client and `local` is the implicit embedded host. This
-- registry records the hosts qfs can act on. `local` is seeded here and cannot be removed; a
-- remote host is recorded by `qfs host login <url>` (which performs NO network I/O — the remote
-- PROTOCOL is deferred per ADR §6). A mount's `host` column (project DB, migration #9) references
-- a `name` here.
--
-- SELECTORS + METADATA ONLY — never a secret. `session_ref` is a placeholder for the future t46
-- session token's storage locator; this ticket stores NO token (the login records the host, no
-- credential), so it is always NULL for now.
CREATE TABLE IF NOT EXISTS hosts (
    -- The host name a mount references (`local`, or an operator-chosen alias for a remote).
    name        TEXT PRIMARY KEY,
    -- The base URL for a remote host; NULL for the implicit `local` host.
    url         TEXT,
    -- 'local' (the embedded engine) or 'remote' (a self-hosted or managed qfs server).
    kind        TEXT NOT NULL,
    -- Placeholder for the future session-token locator (t46); NULL until remote auth lands.
    session_ref TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Seed the implicit local host (present without a login; `qfs host logout` refuses it).
INSERT OR IGNORE INTO hosts (name, url, kind) VALUES ('local', NULL, 'local');
