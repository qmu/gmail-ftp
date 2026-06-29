-- Project DB — migration #7 (t66, roadmap M9 — Managed Team / §3.2/§3.3): the BROKERED team-connection
-- registry — the metadata binding a project connection to the qfs Cloud broker that minted its token.
--
-- APPEND-ONLY: migrations #1 (project.sql) … #6 (project_e2e.sql) are FROZEN (the checksum guard
-- forbids editing a shipped migration); this table ships as a NEW version.
--
-- A row records that a `(driver, connection)` is a TEAM connection provisioned through the broker:
-- which `team` it is scoped to, which upstream `provider` the broker holds a client for, the broker's
-- PUBLIC `broker_client_id` (never the client secret), and the upstream `scope`. This is the M9
-- companion to t81's `shared_connection` table — a brokered connection is ALSO project-owned (it gets
-- a `shared_connection` row), and this table adds the brokering provenance (which team / which broker).
--
-- SELECTORS + METADATA ONLY — never a secret. The two secrets in play stay ENCRYPTED elsewhere:
--   * the brokered TOKEN is sealed in `secret_store` (migration #2, t43 envelope), keyed identically;
--   * the broker CLIENT SECRET is the broker's alone — it never reaches the tenant DB at all (the
--     live qfs Cloud broker holds it; this row records only the broker's PUBLIC client id).
-- Like `shared_connection` / `connection_consent`, this carries no key material, so it needs no
-- passphrase to read (the commit resolver consults it on the passphrase-free path BEFORE any decrypt).
CREATE TABLE IF NOT EXISTS broker_connection (
    driver           TEXT NOT NULL,
    connection       TEXT NOT NULL,
    -- The team the brokered token is scoped to (t66). The load-bearing binding — the connection is the
    -- team's; `assert_team_scope` rejects a grant minted for a different team. Metadata, never a token.
    team             TEXT NOT NULL,
    -- The upstream provider key the broker holds a client for (e.g. `google`, `github`, `slack`).
    provider         TEXT NOT NULL,
    -- The broker's PUBLIC OAuth client id (qfs Cloud's registered client) — NOT the client secret.
    broker_client_id TEXT NOT NULL,
    -- The upstream scope the brokered token carries (a §10 hint, never a token).
    scope            TEXT NOT NULL,
    -- The identity (email / federated handle, t45/t56) that provisioned the team connection. Audit
    -- metadata for WHO provisioned it (the §3.3 two-layer trace); never a credential.
    brokered_by      TEXT NOT NULL DEFAULT '',
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    -- Keyed exactly like `secret_store` / `shared_connection`: one brokering row per stored credential.
    PRIMARY KEY (driver, connection)
);
