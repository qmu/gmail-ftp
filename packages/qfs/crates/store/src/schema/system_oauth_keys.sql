-- System DB — migration #5 (roadmap M2, ticket t48): the OAuth authorization-server signing keys.
-- This is the key-publication half of making a qfs server its OWN authorization server (decision C,
-- §4.1). The AS signs ES256 access tokens (t49/t50); t48 ships the keypair storage + the public-JWK
-- publication behind `/jwks.json` + the discovery documents. NO tokens are issued yet.
--
-- APPEND-ONLY: migrations #1 (system.sql), #2 (system_audit.sql), #3 (system_identity.sql) and #4
-- (system_sessions.sql) are FROZEN — the checksum guard forbids editing a shipped migration in
-- place; this ships as a NEW version (#5). The rusqlite store that FILLS these columns lives in the
-- binary-injected `oauth_key_store.rs` (qfs-store owns the connection); this migration only declares
-- the shape.

-- One AS signing keypair, keyed by its RFC 7638 thumbprint `kid`. The PRIVATE key is
-- envelope-encrypted at rest (decision E — the SAME data-key mechanism that protects connection
-- secrets): `private_key_encrypted` is the ES256 private scalar SEALED under the System-DB data-key
-- (DEK) with a fresh per-row nonce prefix; the plaintext private key NEVER lands here. `public_jwk`
-- is the PUBLIC JWK JSON published verbatim at `/jwks.json` (no secret). `status` is 'active' for the
-- one key that signs new tokens, or 'retiring' for an old key still published so tokens it signed
-- verify during a rotation overlap — multiple rows are supported so JWKS can publish both. The
-- rotation TRIGGER (when to mint a new active key + retire the old) is a documented seam, deferred.
CREATE TABLE IF NOT EXISTS oauth_keys (
    kid                   TEXT PRIMARY KEY,
    alg                   TEXT NOT NULL,
    public_jwk            TEXT NOT NULL,
    private_key_encrypted BLOB NOT NULL,
    created_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    status                TEXT NOT NULL DEFAULT 'active'
);

-- Publishing the JWKS scans by `status` (active + retiring); index it so the lookup is not a scan.
CREATE INDEX IF NOT EXISTS oauth_keys_status ON oauth_keys(status);

-- The single-row envelope metadata for the System-DB OAuth data-key: the once-generated DEK wrapped
-- under a passphrase-derived KEK (`wrapped_dek`) + the per-store argon2id `kdf_salt`. Mirrors the
-- Project DB's `secret_meta` (t43) but scoped to the System DB's OAuth keys. The CHECK pins exactly
-- one row. Unlocking is derive_kek(QFS_PASSPHRASE, kdf_salt) + unwrap, at boot.
CREATE TABLE IF NOT EXISTS oauth_key_meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    wrapped_dek BLOB NOT NULL,
    kdf_salt    BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
