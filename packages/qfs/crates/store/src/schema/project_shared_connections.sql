-- Project DB — migration #4 (t81, roadmap M5 — decision U / §3.3): the PROJECT/TEAM-OWNED
-- (shared) connection registry.
--
-- APPEND-ONLY: migrations #1 (project.sql), #2 (project_secrets.sql) and #3 (project_consent.sql)
-- are FROZEN (the checksum guard forbids editing a shipped migration); this table ships as a NEW
-- version.
--
-- A connection is normally USER-OWNED (`owner_scope = me`): the operator who added it owns the
-- credential. A row in THIS table marks a connection as PROJECT/TEAM-OWNED (`owner_scope = project`):
-- the credential belongs to the project and members USE it *as the team*, bounded by the t57
-- actor-policy — NOT by who holds a token (§3.3 two-layer identity). The presence of a row IS the
-- ownership bit; its `scope` is the realm path (`/projects/<proj>/…`, t71) the acting member's
-- actor-policy must grant before the bind resolves the secret (`qfs_secrets::shared_use_gate`).
--
-- SELECTORS + METADATA ONLY — never a secret. The credential itself stays ENCRYPTED in
-- `secret_store` (migration #2); sharing changes WHO MAY USE it, never the at-rest crypto. Like
-- `active_account` / `connection_consent`, this carries no key material, so it needs no passphrase
-- to read (the commit resolver consults it on the passphrase-free path BEFORE any decrypt).
CREATE TABLE IF NOT EXISTS shared_connection (
    driver     TEXT NOT NULL,
    connection TEXT NOT NULL,
    -- The realm path glob (t71) the acting member's actor-policy must grant to USE this connection
    -- (e.g. `/projects/acme/**`). A scope label is a §10 hint, never a token.
    scope      TEXT NOT NULL,
    -- The identity (email / user label, t45) that shared the connection at the project level. Audit
    -- metadata for WHO shared it (the §3.3 two-layer trace); never a credential.
    shared_by  TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    -- Keyed exactly like `secret_store` (the `CredentialKey` model): one ownership row per stored
    -- credential. A `(driver, connection)` is project-owned iff a row exists here.
    PRIMARY KEY (driver, connection)
);
