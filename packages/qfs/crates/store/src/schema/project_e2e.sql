-- Project DB — migration #6 (t80, roadmap M5 — decision U / §4.5): the PER-RECIPIENT (end-to-end)
-- DEK wrap for HIGH-SENSITIVITY connections.
--
-- APPEND-ONLY: migrations #1–#5 are FROZEN (the checksum guard forbids editing a shipped migration);
-- these tables ship as a NEW version.
--
-- The default credential store (t43) wraps a connection's data-key (DEK) under ONE passphrase-derived
-- KEK the SERVER re-derives, so the managed tier can execute a plan unattended (decision C/F). A
-- connection too sensitive for that trust boundary is marked END-TO-END instead: its DEK is wrapped
-- PER RECIPIENT — separately to each authorized member's PUBLIC key (ECDH,
-- `qfs_oauth::wrap_dek_to_recipient`) — so the DEK is recoverable ONLY by a member who holds the
-- matching PRIVATE key, and NOT by the server at rest (the §4.5 threat-3 mitigation). The explicit
-- trade-off (decision U / J): such a connection cannot be used by an autonomous agent unattended — a
-- human recipient must unwrap it (gated by `qfs_secrets::e2e_attendance_gate`).

-- The per-recipient wrapped DEK: one row per (connection, authorized member). The presence of ANY row
-- for a (driver, connection) IS the "this connection is E2E / high-sensitivity" flag (the same
-- presence-is-the-bit pattern `shared_connection` uses for project ownership). Adding a recipient
-- inserts their wrap; REMOVING a recipient DROPS their row, so they can no longer unwrap NEW state
-- (forward security — a removed recipient who already saw a secret is out of scope, like any E2E
-- system). `wrapped_dek` is the opaque ECDH wrap (`WRAP_MAGIC ‖ ephemeral_pub ‖ nonce ‖ ciphertext`);
-- it is NOT decryptable without the member's private key, so the server storing it cannot recover the
-- DEK. `recipient` is the member's identity handle (the `/sys/users` primary email).
CREATE TABLE IF NOT EXISTS e2e_recipient_wrap (
    driver      TEXT NOT NULL,
    connection  TEXT NOT NULL,
    -- The authorized member (the `/sys/users` primary_email). Audit + lookup metadata, not a secret.
    recipient   TEXT NOT NULL,
    -- The DEK wrapped to THIS recipient's public key (ECDH). Opaque ciphertext; not server-decryptable.
    wrapped_dek BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    -- One wrap per (connection, recipient): adding a recipient is an UPSERT of their wrap, removing
    -- one DELETEs their row.
    PRIMARY KEY (driver, connection, recipient)
);

-- The E2E connection's sealed secret VALUE, kept SEPARATE from the server-unwrappable `secret_store`
-- (migration #2) on purpose: an E2E value is sealed under the per-connection DEK that is wrapped ONLY
-- per recipient (above), so it must NOT be reachable through the global passphrase-DEK path. Storing
-- it here means the ONLY DEK material at rest is the per-recipient wraps — the server cannot decrypt
-- this value by itself (the E2E property). `nonce`/`ciphertext` are the AEAD seal under the
-- per-connection DEK (`qfs_secrets::seal`). One sealed value per (driver, connection).
CREATE TABLE IF NOT EXISTS e2e_secret (
    driver     TEXT NOT NULL,
    connection TEXT NOT NULL,
    nonce      BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (driver, connection)
);
