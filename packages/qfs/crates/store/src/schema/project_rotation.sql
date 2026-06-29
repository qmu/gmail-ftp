-- Project DB — migration #5 (t79, roadmap M5 — decision U / §4.5): credential ROTATION &
-- REVOCATION columns on the envelope-encrypted `secret_store`.
--
-- APPEND-ONLY: migrations #1 (project.sql), #2 (project_secrets.sql), #3 (project_consent.sql) and
-- #4 (project_shared_connections.sql) are FROZEN (the checksum guard forbids editing a shipped
-- migration); these columns ship as a NEW version that ALTERs the table forward. ALTER runs exactly
-- once (the migration runner records the applied version), so it never collides with itself.
--
-- The at-rest crypto is UNCHANGED: a rotation re-seals the secret VALUE under the SAME data-key (DEK)
-- and a DEK re-wrap (passphrase change) rewraps the `secret_meta.wrapped_dek` only — neither touches
-- these columns' contract. Both columns are plaintext METADATA (a timestamp), never a secret.

-- When the credential was last RE-MINTED (`qfs connection rotate`): the secret was replaced under the
-- credential-input path (stdin, never a query literal — §4.5) and re-sealed under the DEK. NULL until
-- the first rotation. Plaintext metadata for `connection list` / `/sys/connections` / the audit trail.
ALTER TABLE secret_store ADD COLUMN last_rotated TEXT;

-- When the credential was REVOKED (`qfs connection revoke` — offboarding / compromise). A NON-NULL
-- value marks the connection unresolvable: the bind path REFUSES to decrypt it and the secret is
-- NEVER returned (default-deny — t79). Re-minting the secret (`rotate`) CLEARS this, restoring use.
-- NULL means the connection is live. Plaintext metadata (a timestamp), never a secret.
ALTER TABLE secret_store ADD COLUMN revoked_at TEXT;
