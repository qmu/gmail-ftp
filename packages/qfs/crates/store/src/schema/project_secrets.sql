-- Project DB — migration #2 (t43): the envelope-encrypted credential store.
--
-- APPEND-ONLY: migration #1 (project.sql) is FROZEN (the checksum guard forbids editing a shipped
-- migration); these tables ship as a NEW version. The crypto lives in qfs-secrets' pure `envelope`
-- module; the SQLite `Secrets` backend that fills these columns lives in the binary (it owns the
-- real connection). This migration only declares the shape.

-- One row per stored credential, keyed by (driver, connection) — exactly the `CredentialKey` model.
-- `ciphertext` is the secret value sealed under the project's data-key (DEK) with a fresh per-value
-- `nonce`; the plaintext NEVER lands here. `created_at` is plaintext metadata for `connection list`.
CREATE TABLE IF NOT EXISTS secret_store (
    driver     TEXT NOT NULL,
    connection TEXT NOT NULL,
    nonce      BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (driver, connection)
);

-- The single-row envelope metadata: the once-generated DEK, wrapped under a passphrase-derived KEK
-- (`wrapped_dek`), plus the per-store argon2id `kdf_salt`. The CHECK pins it to exactly one row so
-- the store has one data-key. Unlocking the store is derive_kek(passphrase, kdf_salt) + unwrap.
CREATE TABLE IF NOT EXISTS secret_meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    wrapped_dek BLOB NOT NULL,
    kdf_salt    BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- The persistent `{driver -> active connection}` selection (moved off the old `credentials.active`
-- sidecar). One row per driver; `qfs connection use` UPSERTs (last-writer-wins), the commit resolver
-- SELECTs. Selectors only — never a secret, so it needs no passphrase to read.
CREATE TABLE IF NOT EXISTS active_account (
    driver     TEXT PRIMARY KEY,
    connection TEXT NOT NULL
);
