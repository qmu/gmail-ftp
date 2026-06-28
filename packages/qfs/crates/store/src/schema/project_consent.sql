-- Project DB — migration #3 (t54, roadmap M4): the cloud-connection CONSENT ledger.
--
-- APPEND-ONLY: migrations #1 (project.sql) and #2 (project_secrets.sql) are FROZEN (the checksum
-- guard forbids editing a shipped migration); this table ships as a NEW version.
--
-- One row per `(driver, connection)` records that a signed-in operator GRANTED that cloud connection
-- explicit consent (decision E). This is the load-bearing M4 state the commit-time bind gate
-- consults: a cloud driver (gmail/gdrive/ga/github/slack/objstore/cf) refuses to bind a credential
-- unless a consent row exists for the selected connection.
--
-- SELECTORS + METADATA ONLY — never a secret. The refresh token itself lives ENCRYPTED in
-- `secret_store` (migration #2); this table records only that consent happened, by whom, for which
-- scope, and when. Like `active_account`, it needs no passphrase to read (it carries no key material),
-- so the commit resolver can check it on the passphrase-free path.
CREATE TABLE IF NOT EXISTS connection_consent (
    driver     TEXT NOT NULL,
    connection TEXT NOT NULL,
    -- The identity that granted consent (an email / user label, t45). Metadata for the audit trail
    -- of WHO consented; never a credential.
    subject    TEXT NOT NULL,
    -- The space-delimited OAuth scope the consent was granted for (the driver's minimum scope set).
    -- A scope label is a §10 hint, never a token.
    scope      TEXT NOT NULL DEFAULT '',
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (driver, connection)
);
