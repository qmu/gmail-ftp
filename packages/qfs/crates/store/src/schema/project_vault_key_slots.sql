-- Project DB — migration #10 (EPIC 20260702120000 / ADR 0008 §5 — KeyGuardian):
-- the VAULT-KEY SLOT table, LUKS-style.
--
-- APPEND-ONLY: migrations #1..#9 are FROZEN (the checksum guard forbids editing a shipped
-- migration); this table ships as a NEW version.
--
-- The store's single 32-byte data-key (DEK) is wrapped ONCE PER GUARDIAN and the wraps sit side
-- by side: the passphrase (an argon2id-derived KEK over `kdf_salt`, kind 'passphrase'), the OS
-- keychain (a random KEK held by the platform secret service, kind 'keychain', `kdf_salt` NULL),
-- and later agent / managed-KMS kinds. ANY one slot unlocks the store; enrolling a slot wraps the
-- SAME DEK under one more KEK without re-sealing a single `secret_store` value; revoking deletes
-- a wrap (the last slot is refused in the I/O — a store with no slot is unopenable). t80's
-- `e2e_recipient_wrap` is this mechanism per-member; this table is its guardian-shaped sibling.
--
-- NO SECRET VALUE: `wrapped_dek` is AEAD-wrapped material that reveals nothing without the
-- guardian's KEK, and `kdf_salt` is public per-slot metadata — same at-rest posture as the
-- `secret_meta` row this table SUPERSEDES.
CREATE TABLE IF NOT EXISTS vault_key_slot (
    -- Stable slot id (`qfs vault revoke <slot>` names it).
    slot_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The guardian kind that can produce this slot's KEK: 'passphrase' | 'keychain' (additive).
    guardian_kind TEXT NOT NULL,
    -- The store DEK, AEAD-wrapped under this slot's KEK (the qfs-secrets wrap_dek format).
    wrapped_dek   BLOB NOT NULL,
    -- The per-slot KDF salt for a derived KEK (passphrase); NULL for raw-KEK guardians (keychain).
    kdf_salt      BLOB,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Forward-copy: an existing store's single passphrase wrap (the t43 `secret_meta` row) becomes
-- slot #1, so a pre-v10 store opens with its existing passphrase unchanged. The copied-from row is
-- then DELETED — from v10 on, `vault_key_slot` is the ONE source of truth for the DEK wraps (a
-- stale duplicate wrap in `secret_meta` would keep an old passphrase alive after a rekey). The
-- `secret_meta` TABLE itself stays (migration #2's shipped body is frozen); it is simply empty.
INSERT INTO vault_key_slot (guardian_kind, wrapped_dek, kdf_salt)
    SELECT 'passphrase', wrapped_dek, kdf_salt FROM secret_meta WHERE id = 1;
DELETE FROM secret_meta WHERE id = 1;
